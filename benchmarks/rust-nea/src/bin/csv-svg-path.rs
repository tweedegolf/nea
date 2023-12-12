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
    let path_string = request
        .body
        .lines()
        .filter_map(|line| line.split_once(", "))
        .map(|(x_str, y_str)| {
            (
                x_str.parse::<usize>().unwrap(),
                y_str.parse::<usize>().unwrap(),
            )
        })
        .fold(String::from("M 0 0 L"), |mut a, (x, y)| {
            use std::fmt::Write;
            a.write_fmt(format_args!("{x} {y}")).unwrap();
            a
        });

    let rust_response = format!(
        r#"<svg width="100" height="100" xmlns="http://www.w3.org/2000/svg">
    <path d="{path_string}" stroke="black" fill="transparent"/>
</svg>
"#
    );

    format_response(&rust_response)
}

// main : Str -> Str
// main = \input ->
//     input
//     |> Str.split "\n"
//     |> List.map \line ->
//         when Str.split line ", " is
//             [ xStr, yStr ] -> ( parseNum xStr, parseNum yStr )
//             _ -> crash "invalid input"
//     |> List.walk "M 0 0 L" \accum, (x, y) -> "\(accum)\(Num.toStr x) \(Num.toStr y) "
//     |> \d ->
//         """
//         <svg width="100" height="100" xmlns="http://www.w3.org/2000/svg">
//           <path d="\(d)" stroke="black" fill="transparent"/>
//         </svg>
//         """
