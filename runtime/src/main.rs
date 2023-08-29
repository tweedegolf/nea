use hyper::Request;
use std::{
    io::{Cursor, ErrorKind, Write},
    os::fd::AsRawFd,
};

use runtime::{reactor, BucketIndex, IoResources, QueueIndex};

fn main() {
    env_logger::init();

    let bucket_count = 64;
    let io_resources = IoResources {
        tcp_streams: 1,
        http_connections: 4,
    };

    let executor = runtime::Executor::get_or_init(bucket_count, io_resources);
    let reactor = runtime::reactor::Reactor::get_or_init(bucket_count, io_resources).unwrap();

    let _handle1 = executor.spawn_worker().unwrap();
    // let _handle2 = executor.spawn_worker().unwrap();

    let listener = std::net::TcpListener::bind("127.0.0.1:8080").unwrap();
    println!("listening on 127.0.0.1:8080");

    // accept connections and process them serially
    for (i, stream) in listener.incoming().enumerate() {
        let stream = stream.unwrap();
        stream.set_nonblocking(true).unwrap();
        let fd = stream.as_raw_fd();

        match executor.try_claim() {
            None => {
                // no space in the queue
                log::warn!("main: no space in the queue");
                stream.shutdown(std::net::Shutdown::Both).unwrap();
            }
            Some(index) => executor.execute(index, async move {
                log::info!("new connection {i} (index = {}, fd = {fd})", index.index);

                let queue_index = QueueIndex::from_bucket_index(io_resources, index);
                let tcp_stream = reactor.register(queue_index, stream).unwrap();

                match app(tcp_stream).await {
                    Ok(()) => {}
                    Err(e) => match e.kind() {
                        ErrorKind::NotConnected => {}
                        _ => Err(e).unwrap(),
                    },
                }
            }),
        };

        log::info!("main spawned future for connection {i}");
    }
}

// async fn app(tcp_stream: reactor::TcpStream) -> std::io::Result<()> {
//     let mut buffer = [0; 1024];
//     let n = tcp_stream.read(&mut buffer).await?;
//
//     let input = &buffer[..n];
//     let input = std::str::from_utf8(input).unwrap();
//
//     let mut buffer = [0u8; 1024];
//     let mut response = Cursor::new(&mut buffer[..]);
//     let _ = response.write_all(b"HTTP/1.1 200 OK\r\n");
//     let _ = response.write_all(b"Content-Type: text/html\r\n");
//     let _ = response.write_all(b"\r\n");
//     let _ = response.write_fmt(format_args!("<html>{input}</html>"));
//     let n = response.position() as usize;
//
//     tcp_stream.write(&response.get_ref()[..n]).await?;
//
//     tcp_stream.flush().await?;
//
//     log::info!("handled a request");
//
//     Ok(())
// }

async fn app(tcp_stream: reactor::TcpStream) -> std::io::Result<()> {
    let mut buffer = [0; 1024];
    let n = tcp_stream.read(&mut buffer).await?;

    let input = &buffer[..n];
    let input = std::str::from_utf8(input).unwrap();

    let index = BucketIndex {
        identifier: 0,
        index: 0,
    };

    let url = "http://docs.rs/h2/latest/h2/"
        .parse::<hyper::Uri>()
        .unwrap();
    let host = url.host().expect("uri has no host");
    let port = url.port_u16().unwrap_or(80);

    let mut sender = runtime::Executor::<()>::get()
        .unwrap()
        .handshake(index, host, port)
        .await
        .unwrap();

    log::info!("performed handshake {} {}", index.index, index.identifier);

    let authority = url.authority().unwrap().clone();

    let req = Request::builder()
        .uri(url)
        .header(hyper::header::HOST, authority.as_str())
        .body(String::new())
        .unwrap();

    log::info!("built request");

    let res = sender.send_request(req).await.unwrap();
    let _ = res;

    // Stream the body, writing each frame to stdout as it arrives
    let res: () = res;
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
        eprintln!("allocating {} bytes", layout.size());
        self.0.alloc(layout)
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: std::alloc::Layout) {
        self.0.dealloc(ptr, layout)
    }
}
