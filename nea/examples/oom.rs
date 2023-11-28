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

    let num_str = &string.split_whitespace().skip(1).next().unwrap()[1..];
    let num = num_str.parse::<usize>().unwrap();

    let kbs = num;
    let _ = Vec::<u8>::with_capacity(kbs * 1_000);

    use std::io::Write;
    let mut response = Vec::new();
    let _ = response.write_all(b"HTTP/1.1 200 OK\r\n");
    let _ = response.write_all(b"Content-Type: text/html\r\n");
    let _ = response.write_all(b"\r\n");
    let _ = response.write_fmt(format_args!("successfully allocated {kbs}kb of memory\r\n"));

    let _ = tcp_stream.write(&response).await.unwrap();

    Ok(())
}
