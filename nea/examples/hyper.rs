use std::io::{Cursor, Write};

fn main() -> std::io::Result<()> {
    nea::run_request_handler(hyper_app)
}

async fn hyper_app(
    bucket_index: nea::index::BucketIndex,
    tcp_stream: nea::net::TcpStream,
) -> std::io::Result<()> {
    let mut buffer = [0; 1024];
    let n = tcp_stream.read(&mut buffer).await?;

    let input = &buffer[..n];
    let input = std::str::from_utf8(input).unwrap();

    let url = "http://0.0.0.0:8000/Cargo.toml"
        .parse::<hyper::Uri>()
        .unwrap();
    let host = url.host().expect("uri has no host");
    let port = url.port_u16().unwrap_or(80);

    let mut sender = nea::http1::handshake(bucket_index, host, port)
        .await
        .unwrap();

    log::info!("performed handshake {}", bucket_index.index);

    let req = hyper::Request::builder()
        .header("Host", "example.com")
        .method("GET")
        .body(String::new())
        .unwrap();

    log::info!("built request");

    let mut res = sender.send_request(req).await.unwrap();
    log::info!("sent request");

    // Stream the body, writing each frame to stdout as it arrives
    use http_body_util::BodyExt;
    while let Some(next) = res.frame().await {
        let _frame = next.unwrap();
    }

    // std::mem::forget(res);

    // std::mem::forget(res);
    log::info!("into body");

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
