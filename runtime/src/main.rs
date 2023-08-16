use std::{
    io::{ErrorKind, Write},
    os::fd::{AsRawFd, FromRawFd},
};

use runtime::reactor;

fn main() {
    env_logger::init();

    let executor = runtime::Executor::get();
    let reactor = runtime::reactor::Reactor::get().unwrap();

    let _handle1 = executor.spawn_worker().unwrap();
    // let _handle2 = executor.spawn_worker().unwrap();

    let listener = std::net::TcpListener::bind("127.0.0.1:8080").unwrap();
    println!("listening on 127.0.0.1:8080");

    // accept connections and process them serially
    let mut i = 0;
    for stream in listener.incoming() {
        let stream = stream.unwrap();
        let fd = stream.as_raw_fd();

        match executor.reserve() {
            Err(()) => {
                // no space in the queue
                log::warn!("main: no space in the queue");

                Ok(())
            }
            Ok(index) => executor.execute(index, async move {
                log::info!("new connection {i} (index = {index}, fd = {fd})");

                let tcp_stream = reactor.register(index, stream).unwrap();

                match app(tcp_stream).await {
                    Ok(()) => {}
                    Err(e) => match dbg!(e.kind()) {
                        ErrorKind::NotConnected => {}
                        _ => Err(e).unwrap(),
                    },
                }
            }),
        };

        log::info!("main spawned future for connection {i}");

        i += 1;
    }
}

async fn app(tcp_stream: reactor::TcpStream) -> std::io::Result<()> {
    let mut buffer = [0; 1024];
    let n = tcp_stream.read(&mut buffer).await?;

    let input = &buffer[..n];
    let input = std::str::from_utf8(input).unwrap();

    let mut response = String::new();
    response.push_str("HTTP/1.1 200 OK\r\n");
    response.push_str("Content-Type: text/html\r\n");
    response.push_str("\r\n");
    response.push_str(&format!("<html>{input}</html>"));

    tcp_stream.write(response.as_bytes()).await?;

    tcp_stream.flush().await?;

    log::info!("handled a request");

    Ok(())
}
