#[derive(Clone)]
#[repr(C)]
pub struct Header<'a> {
    pub key: &'a str,
    pub value: &'a str,
}

#[repr(C)]
pub struct Request<'a> {
    pub body: &'a str,
    pub headers: Vec<Header<'a>>,
    pub method: &'a str,
    pub path: &'a str,
    pub version: &'a str,
}

impl<'a> Request<'a> {
    pub fn parse(string: &'a str) -> Option<Self> {
        let (_headers, body) = string.split_once("\r\n\r\n")?;
        let (x, _headers) = string.split_once("\r\n")?;

        let mut it = x.split_whitespace();
        let method = it.next()?;
        let path = it.next()?;
        let version = it.next()?;
        assert!(it.next().is_none());

        let mut headers = Vec::with_capacity(16);
        for line in _headers.lines() {
            let Some((key, value)) = line.split_once(": ") else {
                continue;
            };

            headers.push(Header { key, value });
        }

        Some(Request {
            body,
            headers,
            method,
            path,
            version,
        })
    }
}

pub fn format_response(rust_response: &str) -> Vec<u8> {
    use std::io::Write;

    let mut response = Vec::with_capacity(512);

    let _ = response.write_all(b"HTTP/1.1 200 OK\r\n");
    let _ = response.write_all(b"Content-Type: text/html\r\n");
    let _ = response.write_all(b"\r\n");
    let _ = response.write_all(rust_response.as_bytes());
    let _ = response.write_all(b"\r\n");

    response
}
