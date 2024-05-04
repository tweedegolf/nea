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

fn respond(request: &Request) -> Vec<u8> {
    let n = request.path[1..].parse::<usize>().unwrap();
    let capacity = n * 1024;

    let v = vec![0xAAu8; capacity];

    format_response(&format!("{}", v.len()))
}
