# Nea - NEver Allocate

**Work in Progress**

A webserver with predictable memory use.

## Aim

Web servers are hard to deploy. The high-level languages that most backend developers like (ruby, php, nodejs, etc.) suffer from high memory use and garbage collection pauses. Low-level languages (rust, zig, etc.) make it possible to control performance and memory use, but struggle to provide the ergonomics of the industry-standard high-level languages.

Nea aims to provide both more reliable performance (by being smarter about allocations) and a convenient high-level language experience via the [roc programming language](https://www.roc-lang.org/).

Nothing comes for free though. Nea imposes two serious constraints: each request only gets a fixed amount of memory, and in total there is a fixed amount of such memory regions. Combined, that means that nea can perform just one (very large) allocation at startup, and thereafter will not touch the system allocator again. When a request is done, its memory region is wiped and reused for later requests.

This is, of course, a tradeoff. It assumes that the request handling takes a decent amount of memory and that the request workload is reasonably consistent (not too many outliers). Because the one alloctation is of a fixed size, there is no way to scale an individual nea instance: changes in traffic volume can only be responded to by in- or decreasing the number of nea instances.

But in return you get a very reliable system: peak memory use is average memory use, there will never be any global garbage collection pauses, and when a particular request does exceed its memory limit, only that request gets cancelled (by sending a proper http error).

## Funders

This project is funded by the [NLnet Foundation].

<img style="margin: 1rem 5% 1rem 5%;" src="https://nlnet.nl/logo/banner.svg" alt="Logo NLnet"  width="200px" />

[NLnet Foundation]: https://nlnet.nl/
