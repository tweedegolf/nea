FROM ubuntu:latest

RUN apt-get update && apt-get install -y curl wget libc-dev binutils siege golang-go clang mold

RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y

# Install latest roc nightly
RUN wget https://github.com/roc-lang/roc/releases/download/nightly/roc_nightly-linux_x86_64-latest.tar.gz
RUN tar -xf roc_nightly-linux_x86_64-latest.tar.gz
# Please fix this before 2099...
RUN cd roc_nightly-linux_x86_64-20* && cp roc /usr/local/bin

ENV PATH="/root/.cargo/bin:${PATH}"

COPY . /nea

RUN cd /nea/benchmarks/roc-nea && roc build --optimize csv-svg-path.roc
RUN cd /nea/benchmarks/roc-nea && roc build --optimize send-static-file.roc
RUN cd /nea/benchmarks/roc-nea && roc build --optimize varying-allocations.roc
RUN cd /nea/benchmarks/rust-nea && cargo build --release
RUN cd /nea/benchmarks/rust-tokio && cargo build --release
RUN cd /nea/benchmarks/go && go build csv-svg-path.go
RUN cd /nea/benchmarks/go && go build send-static-file.go
RUN cd /nea/benchmarks/go && go build varying-allocations.go
RUN cd /nea/runner && cargo build --release && cp ../target/release/runner /usr/local/bin