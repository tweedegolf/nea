# Nea Benchmarks

## Tokio

```shell
cd rust-tokio

cargo run --example unfavorable --release
cargo run --example average --release
cargo run --example favorable --release

siege -c 4 127.0.0.1:8000
```

## Node

```shell
cd ts-node

# Compile TypeScript
npx tsc

node . unfavorable
node . average
node . favorable

siege -c 4 127.0.0.1:8001
```
