<h1 align="center">
<img width="200px" src="./doc/assets/logo.png" />

[![CI][s0]][l0] ![MIT][s1] [![crates][s2]][l2]
</h1>

[s0]: https://github.com/schwartx/octoproxy/workflows/CI/badge.svg
[l0]: https://github.com/schwartx/octoproxy/actions
[s1]: https://img.shields.io/badge/license-MIT-blue.svg
[s2]: https://img.shields.io/crates/v/octoproxy.svg
[l2]: https://crates.io/crates/octoproxy

<h5 align="center">Octoproxy - A load balancing proxy with mTLS encryption for QUIC and HTTP/2</h1>

> This is a work in progress project and is currently in the development phase and may have unknown issues and potential bugs. Please use it at your own risk.

## About

_Octoproxy_ is a load balancing proxy that draws inspiration from the remarkable abilities of an octopus. Just like an octopus with its multiple arms, Octoproxy efficiently manages incoming client requests and distributes them across multiple backend servers. With its flexible tentacles, Octoproxy dynamically adapts to changing network conditions and intelligently routes traffic to ensure optimal performance and high availability. Similar to how an octopus uses its keen senses to navigate the ocean, Octoproxy leverages load balancing algorithms and protocols to monitor server health, detect failures, and seamlessly redirect traffic for a smooth and reliable experience. Dive into the world of Octoproxy and experience its efficient and intelligent load balancing capabilities for your applications.

## Features

- **mTLS Transport Encryption**: mTLS(Mutual TLS) encryption for secure communication.
- **HTTP/2 or QUIC Protocol Selection**: Choose between the HTTP/2 or QUIC protocols for communication.
- **Multiplexing with H2/QUIC**: Utilize H2/QUIC for multiplexing, connections management, and reducing TCP connections to remote servers.
- **Support for Multiple Load Balancing Algorithms**: Support various load balancing algorithms sush as Round Robin, Random, Least Loaded Time, Uri Hash, First, (and more on the way).
- **Metrics**: Provide metrics on action connections per backend, used protocols, and connection latency, (and more on the way).
- **TUI**: Offer a command-line terminal UI for administrative tasks, including mangaging backend status(up/down), protocol switching, and restarting backends, and metrics.
- **100% in Rust**.
- **Single Binary**: Deliver the application as a single executable binary.
- **Host Rewriting**: Allow rewriting of the host(e.g. example.com:8080 can be rewritten as google.com:8080).
- **Backend Selection based on Host**: Enable specifying a backend based on the host, bypassing load balancing algorithms.
- **Selective Direct Connection based on Host**: Allow selective direct connection based on the host, without going through the backend proxy.
- **mTLS Certificate Generator**: Provide a convenient tool for generating mTLS certificates.
- **Certificate Basic Infomation Viewer**: Offer a viewer to display basic information about certificate(valid time, SANs, Issuer, etc).


## Exclusions(Will Not Do)

- **User Management**: Since this aimed to be a personal tool for non-commercial use, there are no plans to implement user/account mangement.
- **Traffic Limits**: Due to the same reasons mentioned above, and also because there is currently no implementation for bandwidth/traffic monitoring.
- **Encryption Protocols/Obfuscation other than mTLS**: No.


## Overview

- `client`: The `octoproxy-client` is a load balancing proxy implemented on the client-side.
- `e2e`: The `e2e` provides a client and server for simple testing purposes.
- `easycert`: The `octoproxy-easycert` is a convenient mTLS certificate generation tool.
- `lib`: The `octoproxy-lib` provides foundational common code.
- `server`: The `octoproxy-server` handles client requests on the server-side.
- `tui`: The `octoproxy-tui` is a terminal-based UI for managing and monitoring the client.

## Quickstart

### Build from source

Build with [mimalloc](https://github.com/microsoft/mimalloc):
```bash
cargo build --bin octoproxy --release -F alloc
```

### Usage

From the client side:
```bash
octoproxy client -c config.toml
```

From the server side:
```bash
octoproxy server -c config.toml
```

To generate a client/server certificate using an existing CA certificate with `octoproxy-easycert`
```bash
octoproxy easycert gen --cacert ./ca.crt --cakey ./ca.key --common-name <common name> --san "DNS:<domain name>" --san "IP:<ip adddress>" -o . --days 365 <client/server cert name>
```
Please ensure that you provide valid and appropriate values for the parameters, including at least one Subject Alternative Name (SAN) value as required by the `--san` option.

Example: To generate a certificate for local server use:
```bash
octoproxy easycert gen --cacert ./ca.crt --cakey ./ca.key --common-name server_name --san "DNS:localhost" --san "IP:127.0.0.1" -o . --days 3650 server
```

## Benchmark

Using [hey](https://github.com/rakyll/hey), a load test was conducted with 1 cpu
and 15 workers, executing a total of 20_000 requests and _Octoproxy_ was utilized with
a single backend, employing the HTTP/2 protocol, see config [in details](bench/client.toml).
The purpose of the test was
to compare the performance of requests made through a proxy and direct requests.
**This is a simple benchmark test, with more scenarios on the way.**


|         Metric        |   Proxy   |   Direct   | Diff(Proxy - Direct) |
|-----------------------|-----------|------------|----------------------|
|    total_time/secs    |   7.0639  |   1.655    |       +5.4089        |
|   slowest_time/secs   |   0.0255  |   0.0118   |       +0.0137        |
|   fastest_time/secs   |   0.0017  |   0.0002   |       +0.0015        |
|   average_time/secs   |   0.0053  |   0.0012   |       +0.0041        |
| requests_per_sec/unit | 2830.5859 | 12081.8832 |      -9251.2973      |


## TODO

- Integration testing and End-to-End testing
- Real-Time Bandwith/Traffic Monitoring
- Enhanced Doc
- Improved Handling of Non-CONNECT method http proxy requests
- Support for Sock5.
- Feature-Rich TUI
- Additional and Complex Load Balancing Algorithms

## Inspiration

- [sozu](https://github.com/sozu-proxy/sozu)
- [quinn](https://github.com/quinn-rs/quinn)
- [rust-rpxy](https://github.com/junkurihara/rust-rpxy)
- [hyper](https://github.com/hyperium/hyper)
- [reqwest](https://github.com/seanmonstar/reqwest)
- [hyper-proxy](https://github.com/tafia/hyper-proxy)
- [gitui](https://github.com/extrawurst/gitui)

## License

This project is licensed under the MIT License. See the [LICENSE](LICENSE) file for details.
