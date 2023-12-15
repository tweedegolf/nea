use std::{collections::BTreeMap, error::Error, fmt::Display};

#[derive(Debug)]
pub struct RequestBuf {
    content: String,
}

impl RequestBuf {
    pub fn new(content: String) -> Self {
        RequestBuf { content }
    }

    pub fn as_request(&self) -> Result<Request<'_>, ParseRequestError> {
        Request::from_str(&self.content)
    }
}

#[derive(Debug)]
pub struct Request<'r> {
    pub body: &'r str,
    pub headers: BTreeMap<&'r str, &'r str>,
    pub method: &'r str,
    pub path: &'r str,
    pub version: &'r str,
}

impl<'r> Request<'r> {
    pub fn from_str(content: &'r str) -> Result<Self, ParseRequestError> {
        let (headers, body) = content
            .split_once("\r\n\r\n")
            .ok_or(ParseRequestError::Syntax)?;

        let mut lines = headers.lines();

        let (method, remainder) = lines
            .next()
            .ok_or(ParseRequestError::UnexpectedEof)?
            .split_once(' ')
            .ok_or(ParseRequestError::Syntax)?;

        let (path, version) = remainder.split_once(' ').ok_or(ParseRequestError::Syntax)?;

        let headers: BTreeMap<_, _> = lines
            .map(|line| line.split_once(": ").ok_or(ParseRequestError::Syntax))
            .collect::<Result<_, ParseRequestError>>()?;

        Ok(Request {
            body,
            headers,
            method,
            path,
            version,
        })
    }
}

#[derive(Debug)]
pub enum ParseRequestError {
    UnexpectedEof,
    Syntax,
}

impl Display for ParseRequestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ParseRequestError::UnexpectedEof => {
                write!(f, "unexpected end of file when parsing request")
            }
            ParseRequestError::Syntax => write!(f, "invalid request syntax"),
        }
    }
}

impl Error for ParseRequestError {}
