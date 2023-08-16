use std::{
    io::{Cursor, ErrorKind, Write},
    os::fd::AsRawFd,
};

use runtime::reactor;

fn main() {
    env_logger::init();

    let executor = runtime::Executor::get_or_init();
    let reactor = runtime::reactor::Reactor::get().unwrap();

    let _handle1 = executor.spawn_worker().unwrap();
    // let _handle2 = executor.spawn_worker().unwrap();

    let listener = std::net::TcpListener::bind("127.0.0.1:8080").unwrap();
    println!("listening on 127.0.0.1:8080");

    // accept connections and process them serially
    let mut i = 0;
    for stream in listener.incoming() {
        let stream = stream.unwrap();
        stream.set_nonblocking(true).unwrap();
        let fd = stream.as_raw_fd();

        match executor.reserve() {
            Err(()) => {
                // no space in the queue
                log::warn!("main: no space in the queue");
                stream.shutdown(std::net::Shutdown::Both).unwrap();

                Ok(())
            }
            Ok(index) => executor.execute(index, async move {
                log::info!("new connection {i} (index = {}, fd = {fd})", index.index);

                let tcp_stream = reactor.register(index, stream).unwrap();

                match app(tcp_stream).await {
                    Ok(()) => {}
                    Err(e) => match dbg!(e.kind()) {
                        ErrorKind::NotConnected => {}
                        _ => Err(e).unwrap(),
                    },
                }
            }),
        }
        .unwrap();

        log::info!("main spawned future for connection {i}");

        i += 1;
    }
}

async fn app(tcp_stream: reactor::TcpStream) -> std::io::Result<()> {
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
