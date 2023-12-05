use rust_tokio::{Request, Response};

#[tokio::main]
async fn main() {
    rust_tokio::serve(unfavorable).await.unwrap();
}

pub async fn unfavorable(_request: Request<'_>) -> Response {
    include_str!("../Cargo.toml").into()
}
