use std::fmt::Write;

use rust_tokio::{request::Request, response::Response, server};

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() {
    server::serve(average).await.unwrap();
}

pub async fn average<'r>(request: Request<'r>) -> Response {
    let path = request
        .body
        .lines()
        .map(|line| line.split_once(", ").expect("invalid input"))
        .map(|(x, y)| {
            (
                x.parse::<u32>().expect("invalid input"),
                y.parse::<u32>().expect("invalid input"),
            )
        })
        .fold("M 0 0 L".to_owned(), |mut acc, (x, y)| {
            write!(acc, "{} {} ", x, y).expect("failed to write to string");
            acc
        });

    format!(
        r#"<svg width="100" height="100" xmlns="http://www.w3.org/2000/svg">
    <path d="{path}" stroke="black" fill="transparent"/>
</svg>"#
    )
    .into()
}
