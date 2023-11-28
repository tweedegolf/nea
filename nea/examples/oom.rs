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

    let mbs = num;
    let _ = Vec::<u8>::with_capacity(mbs * 1_000_000);

    Ok(())
}
