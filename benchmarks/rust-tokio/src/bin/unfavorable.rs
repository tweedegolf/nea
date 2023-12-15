use rust_tokio::{request::Request, response::Response, server};

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() {
    server::serve(unfavorable).await.unwrap();
}

pub async fn unfavorable(_request: Request<'_>) -> Response {
    include_str!("../../Cargo.toml").into()
}
