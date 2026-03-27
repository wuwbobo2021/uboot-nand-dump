use std::io;
use std::ops::Range;

/// Possible errors returned from this crate.
#[derive(Debug)]
pub enum Error {
    InvalidConfig(&'static str),
    InvalidPage(&'static str),
    InvalidRange(Range<usize>),
    OutOfRange,
    UbootNotFound,
    UnstableConnection,
    UnexpectedNandInfo(String),
    Io(io::Error),
    Shell(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Debug::fmt(&self, f) // XXX
    }
}

impl std::error::Error for Error {}

impl From<io::Error> for Error {
    fn from(value: io::Error) -> Self {
        Self::Io(value)
    }
}

impl From<serialport::Error> for Error {
    fn from(value: serialport::Error) -> Self {
        let kind = match value.kind() {
            // XXX
            serialport::ErrorKind::NoDevice => io::ErrorKind::ConnectionReset,
            serialport::ErrorKind::InvalidInput => io::ErrorKind::InvalidInput,
            serialport::ErrorKind::Io(kind) => kind,
            serialport::ErrorKind::Unknown => io::ErrorKind::Other,
        };
        Error::Io(io::Error::new(kind, "serialport error"))
    }
}
