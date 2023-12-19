use crate::request;
use std::fmt::{Display, Formatter};
use std::{error, io};

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Debug)]
pub enum Error {
    ParseRequest(request::ParseRequestError),
    Io(io::Error),
    Utf8(std::str::Utf8Error),
}

impl Display for Error {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::ParseRequest(error) => write!(f, "parse request error: {error}"),
            Error::Io(error) => write!(f, "io error: {error}"),
            Error::Utf8(error) => write!(f, "request is not valid utf-8: {error}"),
        }
    }
}

impl error::Error for Error {}

impl From<request::ParseRequestError> for Error {
    fn from(value: request::ParseRequestError) -> Self {
        Error::ParseRequest(value)
    }
}

impl From<io::Error> for Error {
    fn from(value: io::Error) -> Self {
        Error::Io(value)
    }
}

impl From<std::str::Utf8Error> for Error {
    fn from(value: std::str::Utf8Error) -> Self {
        Error::Utf8(value)
    }
}
