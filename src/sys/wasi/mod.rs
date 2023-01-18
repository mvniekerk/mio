//! # Notes
//!
//! The current implementation is somewhat limited. The `Waker` is not
//! implemented, as at the time of writing there is no way to support to wake-up
//! a thread from calling `poll_oneoff`.
//!
//! Furthermore the (re/de)register functions also don't work while concurrently
//! polling as both registering and polling requires a lock on the
//! `subscriptions`.
//!
//! Finally `Selector::try_clone`, required by `Registry::try_clone`, doesn't
//! work. However this could be implemented by use of an `Arc`.
//!
//! In summary, this only (barely) works using a single thread.

use std::cmp::min;
use std::io;
use std::fmt;
use std::sync::{Arc, Mutex};
use std::time::Duration;
#[cfg(target_vendor = "wasmer")]
use ::wasix as wasi;
use std::sync::Condvar;

use std::fs::File;
use std::os::wasi::io::FromRawFd;
use std::convert::TryInto;
use std::io::Write;
use std::ops::{Deref, DerefMut};

#[cfg(all(feature = "net", target_vendor = "unknown"))]
use crate::{Interest, Token};

#[cfg(target_vendor = "unknown")]
cfg_net! {
    pub(crate) mod tcp {
        use std::io;
        use std::net::{self, SocketAddr};

        pub(crate) fn accept(listener: &net::TcpListener) -> io::Result<(net::TcpStream, SocketAddr)> {
            let (stream, addr) = listener.accept()?;
            stream.set_nonblocking(true)?;
            Ok((stream, addr))
        }
    }
}

#[cfg(target_vendor = "wasmer")]
cfg_os_poll! {
    pub(crate) mod sourcefd;
    pub use self::sourcefd::SourceFd;
    
    pub(crate) mod waker;
    pub(crate) use self::waker::Waker;

    cfg_net! {
        mod net;
        pub(crate) mod tcp;
        pub(crate) mod udp;
        pub(crate) mod pipe;
    }
}

/// Internal state that the polling loop uses
struct SelectorState {
    /// Subscriptions (reads events) we're interested in.
    subscriptions: Vec<wasi::Subscription>,
    /// This file is used to wake up the poll selector
    /// and stall it while management actions are taken
    /// (like adding new subscriptions)
    stall: File,
    /// Number of times that the loop has ticked
    /// (resets to zero when the select exits, will wake threads
    ///  when the number changes)
    loop_cnt: u64,
}

impl SelectorState {
    fn remove_fd(&mut self, fd: wasi::Fd) {
        let predicate = |subscription: &wasi::Subscription| {
            // Safety: `subscription.u.tag` defines the type of the union in
            // `subscription.u.u`.
            match subscription.u.tag {
                t if t == wasi::EVENTTYPE_FD_WRITE.raw() => unsafe {
                    subscription.u.u.fd_write.file_descriptor == fd
                },
                t if t == wasi::EVENTTYPE_FD_READ.raw() => unsafe {
                    subscription.u.u.fd_read.file_descriptor == fd
                },
                _ => false,
            }
        };    
        while let Some(index) = self.subscriptions.iter().position(predicate) {
            self.subscriptions.swap_remove(index);
        }
    }
    fn add_fd(
        &mut self, 
        fd: wasi::Fd,
        token: crate::Token,
        interests: crate::Interest
    )
    {
        if interests.is_writable() {
            let subscription = wasi::Subscription {
                userdata: token.0 as wasi::Userdata,
                u: wasi::SubscriptionU {
                    tag: wasi::EVENTTYPE_FD_WRITE.raw(),
                    u: wasi::SubscriptionUU {
                        fd_write: wasi::SubscriptionFdReadwrite {
                            file_descriptor: fd,
                        },
                    },
                },
            };
            self.subscriptions.push(subscription);
        }

        if interests.is_readable() {
            let subscription = wasi::Subscription {
                userdata: token.0 as wasi::Userdata,
                u: wasi::SubscriptionU {
                    tag: wasi::EVENTTYPE_FD_READ.raw(),
                    u: wasi::SubscriptionUU {
                        fd_read: wasi::SubscriptionFdReadwrite {
                            file_descriptor: fd,
                        },
                    },
                },
            };
            self.subscriptions.push(subscription);
        }
    }
}

struct SelectorStall<'a> {
    guard: Option<std::sync::MutexGuard<'a, SelectorState>>,
    condvar: &'a Condvar,
}
impl<'a> SelectorStall<'a>
{
    fn new(selector: &'a Selector) -> Self {
        Self {
            guard: Some(selector.state.0.lock().unwrap()),
            condvar: &selector.state.1
        }
    }
}
impl<'a> Drop
for SelectorStall<'a> {
    fn drop(&mut self) {
        if let Some(mut guard) = self.guard.take() {
            let start = guard.loop_cnt;
            // if the select is not running then we don't need to wait
            if start == 0 {
                return;
            }
            // the select must increment the loop cnt so we know that
            // it has changed what it is polling for - otherwise the
            // select might return a FD that is gone or it could
            // miss a critical wake up event
            while guard.loop_cnt == start {
                let buf: [u8; 8] = 1u64.to_ne_bytes();
                guard.stall.write(&buf).ok();

                guard = self.condvar.wait(guard).unwrap();
            }
        }
    }
}
impl<'a> Deref
for SelectorStall<'a>
{
    type Target = SelectorState;
    fn deref(&self) -> &Self::Target {
        self.guard.as_ref().unwrap().deref()
    }
}
impl<'a> DerefMut
for SelectorStall<'a>
{
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.guard.as_mut().unwrap().deref_mut()
    }
}

/// Unique id for use as `SelectorId`.
#[cfg(all(debug_assertions, feature = "net"))]
static NEXT_ID: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(1);

pub struct Selector {
    #[cfg(all(debug_assertions, feature = "net"))]
    id: usize,
    #[cfg(debug_assertions)]
    has_waker: std::sync::atomic::AtomicBool,
    /// The state machine used by the select call
    state: Arc<(Mutex<SelectorState>, Condvar)>,
}

impl fmt::Debug
for Selector {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "selector")
    }
}

impl Selector {
    pub fn new() -> io::Result<Selector> {
        let fd = unsafe {
            wasi::fd_event(0, 0)
                .map_err(|errno| io::Error::from_raw_os_error(errno.raw() as i32))?
        };
        let fdstat = unsafe {
            wasi::fd_fdstat_get(fd)
                .map_err(|errno| io::Error::from_raw_os_error(errno.raw() as i32))?
        };
    
        let mut flags = fdstat.fs_flags;
        flags |= wasi::FDFLAGS_NONBLOCK;
        unsafe {
            wasi::fd_fdstat_set_flags(fd, flags)
                .map_err(|errno| io::Error::from_raw_os_error(errno.raw() as i32))?
        }
        let file = unsafe { File::from_raw_fd(fd.try_into().unwrap()) };

        let subscriptions = vec![
            wasi::Subscription {
                userdata: u64::MAX,
                u: wasi::SubscriptionU {
                    tag: wasi::EVENTTYPE_FD_READ.raw(),
                    u: wasi::SubscriptionUU {
                        fd_read: wasi::SubscriptionFdReadwrite {
                            file_descriptor: fd,
                        },
                    },
                },
            }
        ];

        Ok(Selector {
            #[cfg(all(debug_assertions, feature = "net"))]
            id: NEXT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            state: Arc::new((Mutex::new(SelectorState {
                    subscriptions,
                    stall: file,
                    loop_cnt: 0,
                }),
                Condvar::new()
            )),
            #[cfg(debug_assertions)]
            has_waker: std::sync::atomic::AtomicBool::new(false),
        })
    }

    pub fn try_clone(&self) -> io::Result<Selector> {
        Ok(
            Selector {
                #[cfg(all(debug_assertions, feature = "net"))]
                id: self.id,
                state: Arc::clone(&self.state),
                #[cfg(debug_assertions)]
                has_waker: std::sync::atomic::AtomicBool::new(self.has_waker.load(
                    std::sync::atomic::Ordering::Acquire)),
            }
        )
    }

    #[cfg(all(debug_assertions, feature = "net"))]
    pub fn id(&self) -> usize {
        self.id
    }

    pub fn select(&self, events: &mut Events, timeout: Option<Duration>) -> io::Result<()> {
        events.clear();

        let mut loop_cnt = 1u64;
        loop
        {
            // If we want to a use a timeout in the `wasi_poll_oneoff()` function
            // we need another subscription to the list.
            let mut subscriptions = {
                let mut state = self.state.0.lock().unwrap();
                state.subscriptions.retain(|sub| sub.u.tag != wasi::EVENTTYPE_CLOCK.raw());
                state.loop_cnt = loop_cnt;
                loop_cnt += 1;
                self.state.1.notify_all();
                state.subscriptions.clone()
            };
            if let Some(timeout) = timeout {
                subscriptions.push(timeout_subscription(timeout));
            }

            // `poll_oneoff` needs the same number of events as subscriptions.
            let length = subscriptions.len();
            events.reserve(length);

            debug_assert!(events.capacity() >= length);

            let res = unsafe {
                // Safety: `poll_oneoff` initialises the `events` for us.
                let res = wasi::poll_oneoff(subscriptions.as_ptr(), events.as_mut_ptr(), length);
                if let Ok(n_events) = res.as_ref() {
                    events.set_len(*n_events);
                } else {
                    events.set_len(0);
                }
                res
            };

            // If this is a stall event
            if let Ok(n_events) = res.as_ref() {
                if *n_events == 1 {
                    if is_stall_event(&events[0]) {
                        continue;
                    }
                }
            }

            // Clear the run count and notify waiters
            {
                let mut state = self.state.0.lock().unwrap();
                state.loop_cnt = 0;
                self.state.1.notify_all();
            }

            // Return the result
            return match res {
                Ok(_) => {
                    // Remove the timeout or stall events.
                    while let Some(index) = events.iter().position(is_internal_event) {
                        events.swap_remove(index);
                    }

                    // Return the events
                    check_errors(&events)
                }
                Err(err) => Err(io_err(err)),
            };
        }
    }

    fn stall<'a>(&'a self) -> SelectorStall<'a> {
        SelectorStall::new(self)
    }

    #[cfg(any(feature = "net", feature = "os-poll"))]
    pub fn register(
        &self,
        fd: wasi::Fd,
        token: crate::Token,
        interests: crate::Interest,
    ) -> io::Result<()> {

        log::trace!(
            "select::register: fd={:?}, token={:?}, interests={:?}",
            fd,
            token,
            interests
        );

        // we stall the select and wake it up so that it can process
        // the new subscriptions
        let mut stall = self.stall();
        stall.add_fd(fd, token, interests);
        Ok(())
    }

    #[cfg(any(feature = "net", feature = "os-poll"))]
    pub fn reregister(
        &self,
        fd: wasi::Fd,
        token: crate::Token,
        interests: crate::Interest,
    ) -> io::Result<()> {
        log::trace!(
            "select::reregister: fd={:?}, token={:?}, interests={:?}",
            fd,
            token,
            interests
        );

        self.deregister(fd)
            .and_then(|()| self.register(fd, token, interests))
    }

    #[cfg(any(feature = "net", feature = "os-poll"))]
    pub fn deregister(&self, fd: wasi::Fd) -> io::Result<()> {
        log::trace!(
            "select::deregister: fd={:?}",
            fd,
        );

        {
            // we stall the select and wake it up so that it can process
            // the removed subscriptions
            let mut stall = self.stall();
            stall.remove_fd(fd);
        }
        Ok(())
    }

    #[cfg(debug_assertions)]
    pub fn register_waker(&self) -> bool {
        self.has_waker.swap(true, std::sync::atomic::Ordering::AcqRel)
    }
}

/// Token used to a add a timeout subscription, also used in removing it again.
const TIMEOUT_TOKEN: wasi::Userdata = wasi::Userdata::max_value();

/// Returns a `wasi::Subscription` for `timeout`.
fn timeout_subscription(timeout: Duration) -> wasi::Subscription {
    wasi::Subscription {
        userdata: TIMEOUT_TOKEN,
        u: wasi::SubscriptionU {
            tag: wasi::EVENTTYPE_CLOCK.raw(),
            u: wasi::SubscriptionUU {
                clock: wasi::SubscriptionClock {
                    id: wasi::CLOCKID_MONOTONIC,
                    // Timestamp is in nanoseconds.
                    timeout: min(wasi::Timestamp::MAX as u128, timeout.as_nanos())
                        as wasi::Timestamp,
                    // Give the implementation another millisecond to coalesce
                    // events.
                    precision: Duration::from_millis(1).as_nanos() as wasi::Timestamp,
                    // Zero means the `timeout` is considered relative to the
                    // current time.
                    flags: 0,
                },
            },
        },
    }
}

fn is_timeout_event(event: &wasi::Event) -> bool {
    event.type_ == wasi::EVENTTYPE_CLOCK && event.userdata == TIMEOUT_TOKEN
}

fn is_stall_event(event: &wasi::Event) -> bool {
    event.userdata == TIMEOUT_TOKEN
}

fn is_internal_event(event: &wasi::Event) -> bool {
    is_timeout_event(event) || is_stall_event(event)
}

/// Check all events for possible errors, it returns the first error found.
fn check_errors(events: &[Event]) -> io::Result<()> {
    for event in events {
        if event.error != wasi::ERRNO_SUCCESS {
            return Err(io_err(event.error));
        }
    }
    Ok(())
}

/// Convert `wasi::Errno` into an `io::Error`.
fn io_err(errno: wasi::Errno) -> io::Error {
    // TODO: check if this is valid.
    io::Error::from_raw_os_error(errno.raw() as i32)
}

pub(crate) type Events = Vec<Event>;

pub(crate) type Event = wasi::Event;

pub(crate) mod event {
    use std::fmt;
    #[cfg(target_vendor = "wasmer")]
    use ::wasix as wasi;

    use crate::sys::Event;
    use crate::Token;

    pub(crate) fn token(event: &Event) -> Token {
        Token(event.userdata as usize)
    }

    pub(crate) fn is_readable(event: &Event) -> bool {
        event.type_ == wasi::EVENTTYPE_FD_READ
    }

    pub(crate) fn is_writable(event: &Event) -> bool {
        event.type_ == wasi::EVENTTYPE_FD_WRITE
    }

    pub(crate) fn is_error(_: &Event) -> bool {
        // Not supported? It could be that `wasi::Event.error` could be used for
        // this, but the docs say `error that occurred while processing the
        // subscription request`, so it's checked in `Select::select` already.
        false
    }

    pub(crate) fn is_read_closed(event: &Event) -> bool {
        event.type_ == wasi::EVENTTYPE_FD_READ
            // Safety: checked the type of the union above.
            && (event.fd_readwrite.flags & wasi::EVENTRWFLAGS_FD_READWRITE_HANGUP) != 0
    }

    pub(crate) fn is_write_closed(event: &Event) -> bool {
        event.type_ == wasi::EVENTTYPE_FD_WRITE
            // Safety: checked the type of the union above.
            && (event.fd_readwrite.flags & wasi::EVENTRWFLAGS_FD_READWRITE_HANGUP) != 0
    }

    pub(crate) fn is_priority(_: &Event) -> bool {
        // Not supported.
        false
    }

    pub(crate) fn is_aio(_: &Event) -> bool {
        // Not supported.
        false
    }

    pub(crate) fn is_lio(_: &Event) -> bool {
        // Not supported.
        false
    }

    pub(crate) fn debug_details(f: &mut fmt::Formatter<'_>, event: &Event) -> fmt::Result {
        debug_detail!(
            TypeDetails(wasi::Eventtype),
            PartialEq::eq,
            wasi::EVENTTYPE_CLOCK,
            wasi::EVENTTYPE_FD_READ,
            wasi::EVENTTYPE_FD_WRITE,
        );

        #[allow(clippy::trivially_copy_pass_by_ref)]
        fn check_flag(got: &wasi::Eventrwflags, want: &wasi::Eventrwflags) -> bool {
            (got & want) != 0
        }
        debug_detail!(
            EventrwflagsDetails(wasi::Eventrwflags),
            check_flag,
            wasi::EVENTRWFLAGS_FD_READWRITE_HANGUP,
        );

        struct EventFdReadwriteDetails(wasi::EventFdReadwrite);

        impl fmt::Debug for EventFdReadwriteDetails {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.debug_struct("EventFdReadwrite")
                    .field("nbytes", &self.0.nbytes)
                    .field("flags", &self.0.flags)
                    .finish()
            }
        }

        f.debug_struct("Event")
            .field("userdata", &event.userdata)
            .field("error", &event.error)
            .field("type", &TypeDetails(event.type_))
            .field("fd_readwrite", &EventFdReadwriteDetails(event.fd_readwrite))
            .finish()
    }
}

cfg_os_poll! {
    cfg_io_source! {
        pub(crate) struct IoSourceState;

        impl IoSourceState {
            pub(crate) fn new() -> IoSourceState {
                IoSourceState
            }

            pub(crate) fn do_io<T, F, R>(&self, f: F, io: &T) -> io::Result<R>
            where
                F: FnOnce(&T) -> io::Result<R>,
            {
                // We don't hold state, so we can just call the function and
                // return.
                f(io)
            }
        }
    }
}
