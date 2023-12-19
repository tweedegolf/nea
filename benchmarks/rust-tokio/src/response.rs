use std::{
    fmt::{Display, Formatter},
    num::NonZeroU16,
};

#[derive(Debug)]
pub struct Response {
    pub content_type: ContentType,
    pub body: String,
    pub status: StatusCode,
}

impl Display for Response {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "HTTP/1.1 {}\r\n", self.status)?;
        write!(f, "Content-Type: {}; charset=utf-8\r\n", self.content_type)?;
        write!(f, "Content-Length: {}\r\n", self.body.len())?;
        write!(f, "\r\n")?;
        write!(f, "{}", self.body)
    }
}

impl From<&str> for Response {
    fn from(value: &str) -> Self {
        Response {
            content_type: ContentType::TEXT_PLAIN,
            body: value.to_owned(),
            status: StatusCode::OK,
        }
    }
}

impl From<String> for Response {
    fn from(value: String) -> Self {
        Response {
            content_type: ContentType::TEXT_PLAIN,
            body: value,
            status: StatusCode::OK,
        }
    }
}

#[derive(Debug)]
#[repr(transparent)]
pub struct ContentType(&'static str);

impl ContentType {
    pub const TEXT_PLAIN: ContentType = ContentType("text/plain");
    pub const IMAGE_SVG: ContentType = ContentType("image/svg+xml");
}

impl Display for ContentType {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[derive(Debug)]
#[repr(transparent)]
pub struct StatusCode(NonZeroU16);

impl StatusCode {
    pub const OK: StatusCode = StatusCode(match NonZeroU16::new(200) {
        None => unreachable!(),
        Some(n) => n,
    });

    fn desc(&self) -> &'static str {
        match self.0.get() {
            200 => "OK",
            _ => "Unknown",
        }
    }
}

impl Display for StatusCode {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} {}", self.0, self.desc())
    }
}
