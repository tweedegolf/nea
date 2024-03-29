use std::os::raw::{c_char, c_void};
use std::ptr::NonNull;

use roc_std::{RocList, RocStr};

extern "C" {
    #[link_name = "roc__mainForHost_1_exposed"]
    fn mainForHost(input: &Request) -> RocStr;
}

#[no_mangle]
pub unsafe extern "C" fn roc_panic(message_ptr: *const i8, panic_tag: u32) -> ! {
    nea::roc_panic(message_ptr, panic_tag)
}

#[no_mangle]
pub unsafe extern "C" fn roc_dbg(loc: *mut RocStr, msg: *mut RocStr, src: *mut RocStr) {
    eprintln!("[{}] {} = {}", &*loc, &*src, &*msg);
}

#[no_mangle]
pub unsafe extern "C" fn roc_alloc(size: usize, alignment: u32) -> NonNull<u8> {
    nea::roc_alloc(size, alignment)
}

#[no_mangle]
pub unsafe extern "C" fn roc_realloc(
    c_ptr: *mut u8,
    new_size: usize,
    old_size: usize,
    alignment: u32,
) -> NonNull<u8> {
    nea::roc_realloc(c_ptr, new_size, old_size, alignment)
}

#[no_mangle]
pub unsafe extern "C" fn roc_dealloc(c_ptr: *mut u8, alignment: u32) {
    nea::roc_dealloc(c_ptr, alignment)
}

#[cfg(unix)]
#[no_mangle]
pub unsafe extern "C" fn roc_getppid() -> libc::pid_t {
    libc::getppid()
}

#[cfg(unix)]
#[no_mangle]
pub unsafe extern "C" fn roc_mmap(
    addr: *mut libc::c_void,
    len: libc::size_t,
    prot: libc::c_int,
    flags: libc::c_int,
    fd: libc::c_int,
    offset: libc::off_t,
) -> *mut libc::c_void {
    libc::mmap(addr, len, prot, flags, fd, offset)
}

#[cfg(unix)]
#[no_mangle]
pub unsafe extern "C" fn roc_shm_open(
    name: *const libc::c_char,
    oflag: libc::c_int,
    mode: libc::mode_t,
) -> libc::c_int {
    libc::shm_open(name, oflag, mode as libc::c_uint)
}

#[no_mangle]
pub extern "C" fn rust_main() -> i32 {
    match nea::run_request_handler(handler) {
        Ok(_) => 0,
        Err(_) => 1,
    }
}

#[no_mangle]
pub unsafe extern "C" fn roc_memset(dst: *mut c_void, c: i32, n: usize) -> *mut c_void {
    libc::memset(dst, c, n)
}

#[derive(Clone)]
#[repr(C)]
struct Header {
    key: RocStr,
    value: RocStr,
}

#[derive(Default)]
#[repr(C)]
struct Request {
    body: RocStr,
    headers: RocList<Header>,
    method: RocStr,
    path: RocStr,
    version: RocStr,
}

impl Request {
    // TODO we can turn the whole request into a RocStr and then use seamless slices to be a
    // bit more efficient here. But most of these fields will be small strings anyway so the
    // cost is low (also allocation is dirt-cheap with our allocator, so this is probably fine)
    pub fn parse(string: &RocStr) -> Option<Self> {
        let (_headers, body) = string.split_once("\r\n\r\n")?;
        let (x, _headers) = string.split_once("\r\n")?;

        let mut it = x.split_whitespace();
        let method = it.next()?;
        let path = it.next()?;
        let version = it.next()?;
        assert!(it.next().is_none());

        let mut headers = RocList::with_capacity(16);
        for line in _headers.lines() {
            let Some((key, value)) = line.split_once(": ") else {
                continue;
            };

            let key = RocStr::from(key);
            let value = RocStr::from(value);

            headers.push(Header { key, value });
        }

        Some(Request {
            body,
            headers,
            method: RocStr::from(method),
            path: RocStr::from(path),
            version: RocStr::from(version),
        })
    }
}

pub fn format_response(rust_response: &str) -> Vec<u8> {
    use std::io::Write;

    let mut response = Vec::with_capacity(4096);

    let _ = response.write_all(b"HTTP/1.1 200 OK\r\n");
    let _ = response.write_all(b"Content-Type: text/html\r\n");
    let _ = response.write_all(b"\r\n");
    let _ = response.write_all(rust_response.as_bytes());
    let _ = response.write_all(b"\r\n");

    response
}

async fn handler(
    _bucket_index: nea::index::BucketIndex,
    tcp_stream: nea::net::TcpStream,
) -> std::io::Result<()> {
    let mut buf = [0; 4096];

    // leaving 8 zero bytes for future RocStr optimization
    let n = tcp_stream.read(&mut buf[8..]).await.unwrap();

    // perform utf8 validation
    std::str::from_utf8(&buf[8..][..n]).unwrap();

    assert!(8 + n < buf.len());

    let response = {
        // # Safety
        //
        // - this is valid utf-8
        // - there is a refcount in front of the pointer
        // -
        let roc_str = unsafe { RocStr::from_raw_parts(buf[8..].as_mut_ptr().cast(), n, n) };

        let input = Request::parse(&roc_str).unwrap();

        let roc_response = unsafe { mainForHost(&input) };
        std::mem::forget(input);

        format_response(&roc_response)
    };

    let _ = tcp_stream.write(&response).await.unwrap();

    Ok(())
}
