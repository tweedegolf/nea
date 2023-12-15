# Nea Benchmarks

```shell
siege -c 4 -b -i -f urls.txt
```

## Rust / Tokio

```shell
cd rust-tokio

cargo run --bin unfavorable --release
cargo run --bin average --release
cargo run --bin favorable --release
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
