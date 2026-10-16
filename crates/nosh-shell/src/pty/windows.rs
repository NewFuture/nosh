//! User commands inherit the Windows console; ConPTY recording is not implemented.

use std::io;
use std::process::Command;

use crate::user_output::CapturedOutput;

fn unavailable() -> io::Error {
    io::Error::new(
        io::ErrorKind::Unsupported,
        "user terminal output capture requires ConPTY and is not available on Windows",
    )
}

#[derive(Clone)]
pub struct Control(std::convert::Infallible);

impl Control {
    pub fn begin(&self, _: u64) -> io::Result<()> {
        match self.0 {}
    }

    pub fn finish(&self, _: u64) -> io::Result<CapturedOutput> {
        match self.0 {}
    }
}

pub fn flush_input() -> io::Result<()> {
    Ok(())
}

/// # Safety
/// Must be called before runtime threads, like the Unix bootstrap.
pub unsafe fn inherited_control() -> io::Result<Option<Control>> {
    if std::env::var_os("NOSH_INTERNAL_PTY_FD").is_some() {
        return Err(unavailable());
    }
    Ok(None)
}

pub struct SessionPty(std::convert::Infallible);

impl SessionPty {
    pub fn spawn(_: &mut Command) -> io::Result<Self> {
        Err(unavailable())
    }

    pub fn run(self) -> io::Result<i32> {
        match self.0 {}
    }
}
