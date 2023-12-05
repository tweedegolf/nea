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

#[repr(C)]
struct Request {
    body: RocStr,
    headers: RocList<Header>,
    method: RocStr,
    path: RocStr,
    version: RocStr,
}

async fn handler(
    _bucket_index: nea::index::BucketIndex,
    tcp_stream: nea::net::TcpStream,
) -> std::io::Result<()> {
    let mut buf = [0; 1024];

    // leaving 8 zero bytes for future RocStr optimization
    let n = tcp_stream.read(&mut buf[8..]).await.unwrap();
    let string = std::str::from_utf8(&buf[8..][..n]).unwrap();

    let Some((_headers, body)) = string.split_once("\r\n\r\n") else {
        eprintln!("invalid input");
        return Ok(());
    };

    let Some((x, _headers)) = string.split_once("\r\n") else {
        eprintln!("invalid input");
        return Ok(());
    };

    let mut it = x.split_whitespace();
    let method = it.next().unwrap();
    let path = it.next().unwrap();
    let version = it.next().unwrap();
    assert!(it.next().is_none());

    let mut response = Vec::with_capacity(512);

    {
        use std::io::Write;

        // TODO we can turn the whole request into a RocStr and then use seamless slices to be a
        // bit more efficient here. But most of these fields will be small strings anyway so the
        // cost is low (also allocation is dirt-cheap with our allocator, so this is probably fine)

        let mut headers = RocList::with_capacity(16);
        for line in _headers.lines() {
            let Some((key, value)) = line.split_once(": ") else {
                continue;
            };

            let key = RocStr::from(key);
            let value = RocStr::from(value);

            headers.push(Header { key, value });
        }

        let input = Request {
            body: RocStr::from(body),
            headers,
            method: RocStr::from(method),
            path: RocStr::from(path),
            version: RocStr::from(version),
        };

        let roc_response = unsafe { mainForHost(&input) };
        std::mem::forget(input);

        let _ = response.write_all(b"HTTP/1.1 200 OK\r\n");
        let _ = response.write_all(b"Content-Type: text/html\r\n");
        let _ = response.write_all(b"\r\n");
        let _ = response.write_all(roc_response.as_bytes());
        let _ = response.write_all(b"\r\n");
    }

    let _ = tcp_stream.write(&response).await.unwrap();

    Ok(())
}
