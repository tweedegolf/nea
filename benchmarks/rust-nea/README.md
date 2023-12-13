# rust + nea

Benchmarks the performance of a nea platform using a (statically linked) rust request handler.

## Running

The examples can be run with cargo. For performance benchmarking, it is essential to limit the number of log messages that is printed (the lock on stderr is very contested) by setting the `RUST_LOG=error` environment variable for the command.

```
# a debug build that prints many log messages 
cargo run --bin send-static-file

# only print error log messages; used for benchmarking
RUST_LOG=error cargo run --release --bin send-static-file
```

Even when only error messages are configured, some log messages may be visible. These are printed before `main` runs and configures the log level. On linux, this is the expected output:

```
> RUST_LOG=error cargo run --release --bin send-static-file
[2023-12-13T14:49:42Z INFO  nea] bucket ∞: allocating 5 bytes
[2023-12-13T14:49:42Z INFO  nea] bucket ∞: allocating 48 bytes
[2023-12-13T14:49:42Z INFO  nea] bucket ∞: allocating 5 bytes
[2023-12-13T14:49:42Z INFO  nea] bucket ∞: allocating 5 bytes
listening on http://127.0.0.1:8080 on thread ThreadId(1)
```

## Testing & Benchmarking

To just validate an endpoint, we can use curl 

```
> curl -i 127.0.0.1:8080
HTTP/1.1 200 OK
Content-Type: text/html

[workspace]
members = [ 
    "log",
    "shared",
    "roc_app",
    "nea",
]
exclude = [
    "benchmarks",
]
resolver = "2"
```

For performance benchmarking the `siege` tool can be used


```
> siege -c 4 127.0.0.1:8080
** SIEGE 4.0.4
** Preparing 4 concurrent users for battle.
The server is now under siege...^C
Lifting the server siege...
Transactions:		      110826 hits
Availability:		      100.00 %
Elapsed time:		        3.65 secs
Data transferred:	       13.42 MB
Response time:		        0.00 secs
Transaction rate:	    30363.29 trans/sec
Throughput:		        3.68 MB/sec
Concurrency:		        3.12
Successful transactions:      110826
Failed transactions:	           0
Longest transaction:	        0.01
Shortest transaction:	        0.00
```
