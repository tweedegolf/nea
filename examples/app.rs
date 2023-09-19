use hyper::Request;
use std::io::{Cursor, Write};

use runtime::{reactor, BucketIndex, Nea};

fn main() {
    if let Err(error) = Nea::new(app).run() {
        eprintln!("{error}");
    }
}

#[allow(unused)]
async fn app(_bucket_index: BucketIndex, tcp_stream: reactor::TcpStream) -> std::io::Result<()> {
    let mut buffer = [0; 1024];
    let n = tcp_stream.read(&mut buffer).await?;

    let input = &buffer[..n];
    let input = std::str::from_utf8(input).unwrap();

    let mut buffer = [0u8; 1024];
    let mut response = Cursor::new(&mut buffer[..]);
    let _ = response.write_all(b"HTTP/1.1 200 OK\r\n");
    let _ = response.write_all(b"Content-Type: text/html\r\n");
    let _ = response.write_all(b"\r\n");
    let _ = response.write_fmt(format_args!("<html>{input}</html>"));
    let n = response.position() as usize;

    tcp_stream.write(&response.get_ref()[..n]).await?;

    tcp_stream.flush().await?;

    log::info!("handled a request");

    Ok(())
}

#[allow(unused)]
async fn hyper_app(
    bucket_index: BucketIndex,
    tcp_stream: reactor::TcpStream,
) -> std::io::Result<()> {
    let mut buffer = [0; 1024];
    let n = tcp_stream.read(&mut buffer).await?;

    let input = &buffer[..n];
    let input = std::str::from_utf8(input).unwrap();

    let url = "https://github.com/hyperium/hyper/blob/master/examples/client.rs"
        .parse::<hyper::Uri>()
        .unwrap();
    let host = url.host().expect("uri has no host");
    let port = url.port_u16().unwrap_or(80);

    let mut sender = runtime::Executor::<()>::get()
        .unwrap()
        .handshake(bucket_index, host, port)
        .await
        .unwrap();

    log::info!("performed handshake {}", bucket_index.index);

    let authority = url.authority().unwrap().clone();

    let req = Request::builder()
        .header("Host", "example.com")
        .method("GET")
        .body(String::new())
        .unwrap();

    log::info!("built request");

    let res = sender.send_request(req).await.unwrap();
    log::info!("sent request");

    // Stream the body, writing each frame to stdout as it arrives
    dbg!(res.into_body());

    let mut buffer = [0u8; 1024];
    let mut response = Cursor::new(&mut buffer[..]);
    let _ = response.write_all(b"HTTP/1.1 200 OK\r\n");
    let _ = response.write_all(b"Content-Type: text/html\r\n");
    let _ = response.write_all(b"\r\n");
    let _ = response.write_fmt(format_args!("<html>{input}</html>"));
    let n = response.position() as usize;

    tcp_stream.write(&response.get_ref()[..n]).await?;

    tcp_stream.flush().await?;

    log::info!("handled a request");

    Ok(())
}

#[global_allocator]
static ALLOCATOR: LogSystem = LogSystem(std::alloc::System);

struct LogSystem(std::alloc::System);

unsafe impl std::alloc::GlobalAlloc for LogSystem {
    unsafe fn alloc(&self, layout: std::alloc::Layout) -> *mut u8 {
        // eprintln!("allocating {} bytes", layout.size());
        self.0.alloc(layout)
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: std::alloc::Layout) {
        self.0.dealloc(ptr, layout)
    }
}
