use std::hint;

use rust_tokio::{request::RequestBuf, response::Response, server};

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() {
    server::serve(favorable).await.unwrap();
}

pub async fn favorable(request: RequestBuf) -> Response {
    let request = request.as_request().unwrap();

    match request.path.split_once('/') {
        None => panic!("invalid input"),
        Some((_, after)) => {
            let capacity = after.parse::<usize>().unwrap() * 1024;
            let x: Vec<u8> = hint::black_box(Vec::with_capacity(capacity));
            x.len().to_string().into()
        }
    }
}
