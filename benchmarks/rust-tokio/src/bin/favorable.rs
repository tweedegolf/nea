use std::hint;

use rust_tokio::{request::Request, response::Response, server};

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() {
    server::serve(favorable).await.unwrap();
}

pub async fn favorable(_request: Request<'_>) -> Response {
    let mut vecs = hint::black_box(Vec::new());

    for i in 0..100 {
        let vec5000: Vec<u8> = hint::black_box(vec![0xaa; 5000 + i]);
        let vec10000: Vec<u8> = hint::black_box(vec![0xaa; 10000]);
        vecs.push(vec10000);
        drop(vec5000);
    }

    "Hello, World!".into()
}
