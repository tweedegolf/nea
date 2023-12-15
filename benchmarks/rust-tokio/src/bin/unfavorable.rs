use rust_tokio::{request::RequestBuf, response::Response, server};

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() {
    server::serve(unfavorable).await.unwrap();
}

pub async fn unfavorable(request: RequestBuf) -> Response {
    let _request = request.as_request().unwrap();

    include_str!("../../../../Cargo.toml").into()
}
