# Nea Benchmarks

```shell
siege -c 4 -b -i -f urls.txt
```

## Rust / Tokio

```shell
cd rust-tokio

cargo run --example unfavorable --release
cargo run --example average --release
cargo run --example favorable --release
```

## TypeScript / Node

```shell
cd ts-node

npm install
npx tsc

export UV_THREADPOOL_SIZE=2

node . unfavorable
node . average
node . favorable
```
