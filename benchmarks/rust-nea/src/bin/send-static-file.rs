use rust_nea::{format_response, Request};

fn main() -> std::io::Result<()> {
    nea::run_request_handler(handler)
}

async fn handler(
    _bucket_index: nea::index::BucketIndex,
    tcp_stream: nea::net::TcpStream,
) -> std::io::Result<()> {
    let mut buf = [0; 1024];

    // leaving 8 zero bytes for future RocStr optimization
    let n = tcp_stream.read(&mut buf).await.unwrap();
    let string = std::str::from_utf8(&buf[..n]).unwrap();

    let request = Request::parse(string).unwrap();
    let response = respond(&request);

    let _ = tcp_stream.write(&response).await.unwrap();

    Ok(())
}

fn respond(_request: &Request) -> Vec<u8> {
    let rust_response = include_str!("/home/folkertdev/rust/nea/Cargo.toml");
    format_response(rust_response)
}
