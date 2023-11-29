fn main() -> std::io::Result<()> {
    nea::run_request_handler(handler)
}

async fn handler(
    _bucket_index: nea::index::BucketIndex,
    tcp_stream: nea::net::TcpStream,
) -> std::io::Result<()> {
    let mut buf = [0; 1024];

    let n = tcp_stream.read(&mut buf).await.unwrap();

    let string = std::str::from_utf8(&buf[..n]).unwrap();

    let Some((_headers, body)) = string.split_once("\r\n\r\n") else {
        eprintln!("invalid input");
        return Ok(());
    };

    let mut response = Vec::with_capacity(512);

    {
        use std::io::Write;

        let roc_response = main_for_host(body);

        let _ = response.write_all(b"HTTP/1.1 200 OK\r\n");
        let _ = response.write_all(b"Content-Type: text/html\r\n");
        let _ = response.write_all(b"\r\n");
        let _ = response.write_all(roc_response.as_bytes());
        let _ = response.write_all(b"\r\n");
    }

    let _ = tcp_stream.write(&response).await.unwrap();

    Ok(())
}

fn main_for_host(_: &str) -> &'static str {
    include_str!("/home/folkertdev/rust/nea/platform/Cargo.toml")
}
