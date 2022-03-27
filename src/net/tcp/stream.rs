use std::fmt;
use std::io::{self, IoSlice, IoSliceMut, Read, Write};
use std::net::{self, Shutdown, SocketAddr};
#[cfg(unix)]
use std::os::unix::io::{AsRawFd, FromRawFd, IntoRawFd, RawFd};
#[cfg(target_os = "wasi")]
use std::os::wasi::io::{AsRawFd, FromRawFd, IntoRawFd, RawFd};
#[cfg(target_os = "wasi")]
use wasm_bus_mio::prelude as wasm_bus_mio;
#[cfg(windows)]
use std::os::windows::io::{AsRawSocket, FromRawSocket, IntoRawSocket, RawSocket};

use crate::io_source::IoSource;
#[cfg(not(target_os = "wasi"))]
use crate::sys::tcp::{connect, new_for_addr};
use crate::{event, Interest, Registry, Token};

/// A non-blocking TCP stream between a local socket and a remote socket.
///
/// The socket will be closed when the value is dropped.
///
/// # Examples
///
#[cfg_attr(feature = "os-poll", doc = "```")]
#[cfg_attr(not(feature = "os-poll"), doc = "```ignore")]
/// # use std::net::{TcpListener, SocketAddr};
/// # use std::error::Error;
/// #
/// # fn main() -> Result<(), Box<dyn Error>> {
/// let address: SocketAddr = "127.0.0.1:0".parse()?;
/// let listener = TcpListener::bind(address)?;
/// use mio::{Events, Interest, Poll, Token};
/// use mio::net::TcpStream;
/// use std::time::Duration;
///
/// let mut stream = TcpStream::connect(listener.local_addr()?)?;
///
/// let mut poll = Poll::new()?;
/// let mut events = Events::with_capacity(128);
///
/// // Register the socket with `Poll`
/// poll.registry().register(&mut stream, Token(0), Interest::WRITABLE)?;
///
/// poll.poll(&mut events, Some(Duration::from_millis(100)))?;
///
/// // The socket might be ready at this point
/// #     Ok(())
/// # }
/// ```
pub enum TcpStream {
    /// Inner std TCP stream
    Inner(IoSource<net::TcpStream>),
    /// WASM bus interface
    #[cfg(target_os = "wasi")]
    WasmBus(wasm_bus_mio::TcpStream)
}

impl TcpStream {
    /// Create a new TCP stream and issue a non-blocking connect to the
    /// specified address.
    ///
    /// # Notes
    ///
    /// The returned `TcpStream` may not be connected (and thus usable), unlike
    /// the API found in `std::net::TcpStream`. Because Mio issues a
    /// *non-blocking* connect it will not block the thread and instead return
    /// an unconnected `TcpStream`.
    ///
    /// Ensuring the returned stream is connected is surprisingly complex when
    /// considering cross-platform support. Doing this properly should follow
    /// the steps below, an example implementation can be found
    /// [here](https://github.com/Thomasdezeeuw/heph/blob/0c4f1ab3eaf08bea1d65776528bfd6114c9f8374/src/net/tcp/stream.rs#L560-L622).
    ///
    ///  1. Call `TcpStream::connect`
    ///  2. Register the returned stream with at least [read interest].
    ///  3. Wait for a (readable) event.
    ///  4. Check `TcpStream::peer_addr`. If it returns `libc::EINPROGRESS` or
    ///     `ErrorKind::NotConnected` it means the stream is not yet connected,
    ///     go back to step 3. If it returns an address it means the stream is
    ///     connected, go to step 5. If another error is returned something
    ///     whent wrong.
    ///  5. Now the stream can be used.
    ///
    /// [read interest]: Interest::READABLE
    pub fn connect(addr: SocketAddr) -> io::Result<TcpStream> {
        #[cfg(not(target_os = "wasi"))]
        let socket = new_for_addr(addr)?;
        #[cfg(unix)]
        let stream = unsafe { TcpStream::from_raw_fd(socket) };
        #[cfg(windows)]
        let stream = unsafe { TcpStream::from_raw_socket(socket as _) };
        #[cfg(target_os = "wasi")]
        let stream = TcpStream::WasmBus(wasm_bus_mio::TcpStream::connect(addr)?);

        match stream {
            #[allow(unused_variables)]
            TcpStream::Inner(ref inner) => {
                #[cfg(not(target_os = "wasi"))]
                connect(inner, addr)?;
            }
            #[cfg(target_os = "wasi")]
            TcpStream::WasmBus(_) => {
            }
        }
        Ok(stream)
    }

    /// Creates a new `TcpStream` from a standard `net::TcpStream`.
    ///
    /// This function is intended to be used to wrap a TCP stream from the
    /// standard library in the Mio equivalent. The conversion assumes nothing
    /// about the underlying stream; it is left up to the user to set it in
    /// non-blocking mode.
    ///
    /// # Note
    ///
    /// The TCP stream here will not have `connect` called on it, so it
    /// should already be connected via some other means (be it manually, or
    /// the standard library).
    pub fn from_std(stream: net::TcpStream) -> TcpStream {
        TcpStream::Inner(IoSource::new(stream))
    }

    /// Returns the socket address of the remote peer of this TCP connection.
    pub fn peer_addr(&self) -> io::Result<SocketAddr> {
        match self {
            TcpStream::Inner(inner) => {
                inner.peer_addr()
            }
            #[cfg(target_os = "wasi")]
            TcpStream::WasmBus(bus) => {
                bus.peer_addr()
            }
        }
    }

    /// Returns the socket address of the local half of this TCP connection.
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        match self {
            TcpStream::Inner(inner) => {
                inner.local_addr()
            }
            #[cfg(target_os = "wasi")]
            TcpStream::WasmBus(bus) => {
                bus.local_addr()
            }
        }
    }

    /// Shuts down the read, write, or both halves of this connection.
    ///
    /// This function will cause all pending and future I/O on the specified
    /// portions to return immediately with an appropriate value (see the
    /// documentation of `Shutdown`).
    pub fn shutdown(&self, how: Shutdown) -> io::Result<()> {
        match self {
            TcpStream::Inner(inner) => {
                inner.shutdown(how)
            }
            #[cfg(target_os = "wasi")]
            TcpStream::WasmBus(bus) => {
                bus.shutdown(how)
            }
        }
    }

    /// Sets the value of the `TCP_NODELAY` option on this socket.
    ///
    /// If set, this option disables the Nagle algorithm. This means that
    /// segments are always sent as soon as possible, even if there is only a
    /// small amount of data. When not set, data is buffered until there is a
    /// sufficient amount to send out, thereby avoiding the frequent sending of
    /// small packets.
    ///
    /// # Notes
    ///
    /// On Windows make sure the stream is connected before calling this method,
    /// by receiving an (writable) event. Trying to set `nodelay` on an
    /// unconnected `TcpStream` is unspecified behavior.
    pub fn set_nodelay(&self, nodelay: bool) -> io::Result<()> {
        match self {
            TcpStream::Inner(inner) => {
                inner.set_nodelay(nodelay)
            }
            #[cfg(target_os = "wasi")]
            TcpStream::WasmBus(bus) => {
                bus.set_nodelay(nodelay)
            }
        }
    }

    /// Gets the value of the `TCP_NODELAY` option on this socket.
    ///
    /// For more information about this option, see [`set_nodelay`][link].
    ///
    /// [link]: #method.set_nodelay
    ///
    /// # Notes
    ///
    /// On Windows make sure the stream is connected before calling this method,
    /// by receiving an (writable) event. Trying to get `nodelay` on an
    /// unconnected `TcpStream` is unspecified behavior.
    pub fn nodelay(&self) -> io::Result<bool> {
        match self {
            TcpStream::Inner(inner) => {
                inner.nodelay()
            }
            #[cfg(target_os = "wasi")]
            TcpStream::WasmBus(bus) => {
                bus.nodelay()
            }
        }
    }

    /// Sets the value for the `IP_TTL` option on this socket.
    ///
    /// This value sets the time-to-live field that is used in every packet sent
    /// from this socket.
    ///
    /// # Notes
    ///
    /// On Windows make sure the stream is connected before calling this method,
    /// by receiving an (writable) event. Trying to set `ttl` on an
    /// unconnected `TcpStream` is unspecified behavior.
    pub fn set_ttl(&self, ttl: u32) -> io::Result<()> {
        match self {
            TcpStream::Inner(inner) => {
                inner.set_ttl(ttl)
            }
            #[cfg(target_os = "wasi")]
            TcpStream::WasmBus(bus) => {
                bus.set_ttl(ttl)
            }
        }
    }

    /// Gets the value of the `IP_TTL` option for this socket.
    ///
    /// For more information about this option, see [`set_ttl`][link].
    ///
    /// # Notes
    ///
    /// On Windows make sure the stream is connected before calling this method,
    /// by receiving an (writable) event. Trying to get `ttl` on an
    /// unconnected `TcpStream` is unspecified behavior.
    ///
    /// [link]: #method.set_ttl
    pub fn ttl(&self) -> io::Result<u32> {
        match self {
            TcpStream::Inner(inner) => {
                inner.ttl()
            }
            #[cfg(target_os = "wasi")]
            TcpStream::WasmBus(bus) => {
                bus.ttl()
            }
        }
    }

    /// Get the value of the `SO_ERROR` option on this socket.
    ///
    /// This will retrieve the stored error in the underlying socket, clearing
    /// the field in the process. This can be useful for checking errors between
    /// calls.
    pub fn take_error(&self) -> io::Result<Option<io::Error>> {
        match self {
            TcpStream::Inner(inner) => {
                inner.take_error()
            }
            #[cfg(target_os = "wasi")]
            TcpStream::WasmBus(bus) => {
                bus.take_error()
            }
        }
    }

    /// Receives data on the socket from the remote address to which it is
    /// connected, without removing that data from the queue. On success,
    /// returns the number of bytes peeked.
    ///
    /// Successive calls return the same data. This is accomplished by passing
    /// `MSG_PEEK` as a flag to the underlying recv system call.
    pub fn peek(&self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            TcpStream::Inner(inner) => {
                inner.peek(buf)
            }
            #[cfg(target_os = "wasi")]
            TcpStream::WasmBus(bus) => {
                let ret = bus.peek(buf.len())?;
                let size = ret.len();
                buf[..size].copy_from_slice(&ret[..size]);
                Ok(size)
            }
        }
    }

    /// Execute an I/O operation ensuring that the socket receives more events
    /// if it hits a [`WouldBlock`] error.
    ///
    /// # Notes
    ///
    /// This method is required to be called for **all** I/O operations to
    /// ensure the user will receive events once the socket is ready again after
    /// returning a [`WouldBlock`] error.
    ///
    /// [`WouldBlock`]: io::ErrorKind::WouldBlock
    ///
    /// # Examples
    ///
    #[cfg_attr(unix, doc = "```no_run")]
    #[cfg_attr(windows, doc = "```ignore")]
    /// # use std::error::Error;
    /// #
    /// # fn main() -> Result<(), Box<dyn Error>> {
    /// use std::io;
    /// #[cfg(unix)]
    /// use std::os::unix::io::AsRawFd;
    /// #[cfg(windows)]
    /// use std::os::windows::io::AsRawSocket;
    /// use mio::net::TcpStream;
    ///
    /// let address = "127.0.0.1:8080".parse().unwrap();
    /// let stream = TcpStream::connect(address)?;
    ///
    /// // Wait until the stream is readable...
    ///
    /// // Read from the stream using a direct libc call, of course the
    /// // `io::Read` implementation would be easier to use.
    /// let mut buf = [0; 512];
    /// let n = stream.try_io(|| {
    ///     let buf_ptr = &mut buf as *mut _ as *mut _;
    ///     #[cfg(unix)]
    ///     let res = unsafe { libc::recv(stream.as_raw_fd(), buf_ptr, buf.len(), 0) };
    ///     #[cfg(windows)]
    ///     let res = unsafe { libc::recvfrom(stream.as_raw_socket() as usize, buf_ptr, buf.len() as i32, 0, std::ptr::null_mut(), std::ptr::null_mut()) };
    ///     if res != -1 {
    ///         Ok(res as usize)
    ///     } else {
    ///         // If EAGAIN or EWOULDBLOCK is set by libc::recv, the closure
    ///         // should return `WouldBlock` error.
    ///         Err(io::Error::last_os_error())
    ///     }
    /// })?;
    /// eprintln!("read {} bytes", n);
    /// # Ok(())
    /// # }
    /// ```
    pub fn try_io<F, T>(&self, f: F) -> io::Result<T>
    where
        F: FnOnce() -> io::Result<T>,
    {
        match self {
            TcpStream::Inner(inner) => {
                inner.do_io(|_| f())
            }
            #[cfg(target_os = "wasi")]
            TcpStream::WasmBus(_) => {
                panic!("try_io is not supported for wasm_bus_mio");
            }
        }
    }
}

impl Read for TcpStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            TcpStream::Inner(inner) => {
                inner.do_io(|inner| (&*inner).read(buf))
            }
            #[cfg(target_os = "wasi")]
            TcpStream::WasmBus(ref bus) => {
                let ret = bus.read(buf.len())?;
                let size = ret.len();
                buf[..size].copy_from_slice(&ret[..size]);
                Ok(size)
            }
        }
    }

    fn read_vectored(&mut self, bufs: &mut [IoSliceMut<'_>]) -> io::Result<usize> {
        match self {
            TcpStream::Inner(inner) => {
                inner.do_io(|inner| (&*inner).read_vectored(bufs))
            }
            #[cfg(target_os = "wasi")]
            TcpStream::WasmBus(bus) => {
                let mut total = 0;
                for buf in bufs {
                    let ret = bus.read(buf.len())?;
                    let size = ret.len();
                    buf[..size].copy_from_slice(&ret[..size]);
                    total += size;
                }
                Ok(total)
            }
        }
    }
}

impl<'a> Read for &'a TcpStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            TcpStream::Inner(inner) => {
                inner.do_io(|inner| (&*inner).read(buf))
            }
            #[cfg(target_os = "wasi")]
            TcpStream::WasmBus(ref bus) => {
                let ret = bus.read(buf.len())?;
                let size = ret.len();
                buf[..size].copy_from_slice(&ret[..size]);
                Ok(size)
            }
        }
    }

    fn read_vectored(&mut self, bufs: &mut [IoSliceMut<'_>]) -> io::Result<usize> {
        match self {
            TcpStream::Inner(inner) => {
                inner.do_io(|inner| (&*inner).read_vectored(bufs))
            }
            #[cfg(target_os = "wasi")]
            TcpStream::WasmBus(bus) => {
                let mut total = 0;
                for buf in bufs {
                    let ret = bus.read(buf.len())?;
                    let size = ret.len();
                    buf[..size].copy_from_slice(&ret[..size]);
                    total += size;
                }
                Ok(total)
            }
        }
    }
}

impl Write for TcpStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            TcpStream::Inner(inner) => {
                inner.do_io(|inner| (&*inner).write(buf))
            }
            #[cfg(target_os = "wasi")]
            TcpStream::WasmBus(bus) => {
                bus.write(buf.to_vec())
            }
        }
    }

    fn write_vectored(&mut self, bufs: &[IoSlice<'_>]) -> io::Result<usize> {
        match self {
            TcpStream::Inner(inner) => {
                inner.do_io(|inner| (&*inner).write_vectored(bufs))
            }
            #[cfg(target_os = "wasi")]
            TcpStream::WasmBus(bus) => {
                let mut total = 0;
                for buf in bufs {
                    let buf = buf.to_vec();
                    total += bus.write(buf)?;
                }
                Ok(total)
            }
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            TcpStream::Inner(inner) => {
                inner.do_io(|inner| (&*inner).flush())
            }
            #[cfg(target_os = "wasi")]
            TcpStream::WasmBus(bus) => {
                bus.flush()
            }
        }
    }
}

impl<'a> Write for &'a TcpStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            TcpStream::Inner(inner) => {
                inner.do_io(|inner| (&*inner).write(buf))
            }
            #[cfg(target_os = "wasi")]
            TcpStream::WasmBus(bus) => {
                bus.write(buf.to_vec())
            }
        }
    }

    fn write_vectored(&mut self, bufs: &[IoSlice<'_>]) -> io::Result<usize> {
        match self {
            TcpStream::Inner(inner) => {
                inner.do_io(|inner| (&*inner).write_vectored(bufs))
            }
            #[cfg(target_os = "wasi")]
            TcpStream::WasmBus(bus) => {
                let mut total = 0;
                for buf in bufs {
                    let buf = buf.to_vec();
                    total += bus.write(buf)?;
                }
                Ok(total)
            }
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            TcpStream::Inner(inner) => {
                inner.do_io(|inner| (&*inner).flush())
            }
            #[cfg(target_os = "wasi")]
            TcpStream::WasmBus(bus) => {
                bus.flush()
            }
        }
    }
}

impl event::Source for TcpStream {
    fn register(
        &mut self,
        registry: &Registry,
        token: Token,
        interests: Interest,
    ) -> io::Result<()> {
        match self {
            TcpStream::Inner(inner) => {
                inner.register(registry, token, interests)
            }
            #[cfg(target_os = "wasi")]
            TcpStream::WasmBus(_) => {
                Ok(())
            }
        }
    }

    fn reregister(
        &mut self,
        registry: &Registry,
        token: Token,
        interests: Interest,
    ) -> io::Result<()> {
        match self {
            TcpStream::Inner(inner) => {
                inner.reregister(registry, token, interests)
            }
            #[cfg(target_os = "wasi")]
            TcpStream::WasmBus(_) => {
                Ok(())
            }
        }
    }

    fn deregister(&mut self, registry: &Registry) -> io::Result<()> {
        match self {
            TcpStream::Inner(inner) => {
                inner.deregister(registry)
            }
            #[cfg(target_os = "wasi")]
            TcpStream::WasmBus(_) => {
                Ok(())
            }
        }
    }
}

impl fmt::Debug for TcpStream {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TcpStream::Inner(inner) => {
                inner.fmt(f)
            }
            #[cfg(target_os = "wasi")]
            TcpStream::WasmBus(_) => {
                write!(f, "wasm-bus-mio::TcpStream")
            }
        }
    }
}

#[cfg(unix)]
impl IntoRawFd for TcpStream {
    fn into_raw_fd(self) -> RawFd {
        match self {
            TcpStream::Inner(inner) => {
                inner.into_inner().into_raw_fd()
            }
            #[cfg(target_os = "wasi")]
            TcpStream::WasmBus(_) => {
                panic!("WASM Bus TcpStreams can not be converted to raw file descriptors")
            }
        }
    }
}

#[cfg(unix)]
impl AsRawFd for TcpStream {
    fn as_raw_fd(&self) -> RawFd {
        match self {
            TcpStream::Inner(inner) => {
                inner.as_raw_fd()
            }
            #[cfg(target_os = "wasi")]
            TcpStream::WasmBus(_) => {
                panic!("WASM Bus TcpStreams can not be converted to raw file descriptors")
            }
        }
    }
}

#[cfg(unix)]
impl FromRawFd for TcpStream {
    /// Converts a `RawFd` to a `TcpStream`.
    ///
    /// # Notes
    ///
    /// The caller is responsible for ensuring that the socket is in
    /// non-blocking mode.
    unsafe fn from_raw_fd(fd: RawFd) -> TcpStream {
        TcpStream::from_std(FromRawFd::from_raw_fd(fd))
    }
}

#[cfg(windows)]
impl IntoRawSocket for TcpStream {
    fn into_raw_socket(self) -> RawSocket {
        match self {
            TcpStream::Inner(inner) => {
                inner.into_inner().into_raw_socket()
            }
            #[cfg(target_os = "wasi")]
            TcpStream::WasmBus(_) => {
                panic!("WASM Bus TcpStreams can not be converted to raw file descriptors")
            }
        }
    }
}

#[cfg(windows)]
impl AsRawSocket for TcpStream {
    fn as_raw_socket(&self) -> RawSocket {
        match self {
            TcpStream::Inner(inner) => {
                inner.as_raw_socket()
            }
            #[cfg(target_os = "wasi")]
            TcpStream::WasmBus(_) => {
                panic!("WASM Bus TcpStreams can not be converted to raw file descriptors")
            }
        }
    }
}

#[cfg(windows)]
impl FromRawSocket for TcpStream {
    /// Converts a `RawSocket` to a `TcpStream`.
    ///
    /// # Notes
    ///
    /// The caller is responsible for ensuring that the socket is in
    /// non-blocking mode.
    unsafe fn from_raw_socket(socket: RawSocket) -> TcpStream {
        TcpStream::from_std(FromRawSocket::from_raw_socket(socket))
    }
}

#[cfg(target_os = "wasi")]
impl IntoRawFd for TcpStream {
    fn into_raw_fd(self) -> RawFd {
        match self {
            TcpStream::Inner(inner) => {
                inner.into_inner().into_raw_fd()
            }
            #[cfg(target_os = "wasi")]
            TcpStream::WasmBus(_) => {
                panic!("WASM Bus TcpStreams can not be converted to raw file descriptors")
            }
        }
    }
}

#[cfg(target_os = "wasi")]
impl AsRawFd for TcpStream {
    fn as_raw_fd(&self) -> RawFd {
        match self {
            TcpStream::Inner(inner) => {
                inner.as_raw_fd()
            }
            #[cfg(target_os = "wasi")]
            TcpStream::WasmBus(_) => {
                panic!("WASM Bus TcpStreams can not be converted to raw file descriptors")
            }
        }
    }
}

#[cfg(target_os = "wasi")]
impl FromRawFd for TcpStream {
    /// Converts a `RawFd` to a `TcpStream`.
    ///
    /// # Notes
    ///
    /// The caller is responsible for ensuring that the socket is in
    /// non-blocking mode.
    unsafe fn from_raw_fd(fd: RawFd) -> TcpStream {
        TcpStream::from_std(FromRawFd::from_raw_fd(fd))
    }
}
