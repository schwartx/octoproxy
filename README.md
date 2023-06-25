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
