use crate::sys::Selector;
use crate::Token;
use std::io;

#[derive(Debug)]
pub struct Waker {}

impl Waker {
    #[allow(dead_code)]
    pub fn new(_: &Selector, _: Token) -> io::Result<Waker> {
        os_required!();
    }

    #[allow(dead_code)]
    pub fn wake(&self) -> io::Result<()> {
        os_required!();
    }
}
