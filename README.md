# bx402

[![CI](https://github.com/brave-experiments/bx402/actions/workflows/ci.yml/badge.svg)](https://github.com/brave-experiments/bx402/actions/workflows/ci.yml)
[![made-with-rust](https://img.shields.io/badge/Made%20with-Rust-1f425f.svg)](https://www.rust-lang.org/)

A pay-per-request proxy in front of the [Brave Search API](https://brave.com/search/api/).
Instead of an API key, each request carries a stablecoin micropayment, making the signed payment
act as the credential.

`bx402` is `bx` (Brave Search CLI) + `402`, the HTTP *Payment Required* status.
The `402` is the shared mechanism, not a rail, so **x402** (USDC
on Base) and **MPP** (pathUSD on Tempo) are equally first-class.

| Spec | https://gist.github.com/onyb/a1d620ba1e6ded2577a2998f2ecb0f61 |
-|-

## How it works

1. A request with no payment receives one `402 Payment Required` that advertises both rails.
2. The client retries with the header matching its wallet, either x402 or MPP.
3. On a valid payment the request is forwarded to Brave Search and the result is returned
   with the settlement receipt.

One hostname serves both rails. The rail is chosen by the client's payment header.

## Prerequisites

[`rustup`](https://rustup.rs/). The compiler version is pinned in `rust-toolchain.toml` and
installed automatically on first build.

## Quickstart

```sh
git clone git@github.com:brave-experiments/bx402.git
cd bx402
cargo run
cargo test
```

## Development

```sh
cargo fmt --all
cargo clippy --all-targets --all-features
cargo test --all-features
```

CI runs the same three checks on every push and pull request.

## Project layout

```
src/
├── lib.rs    # library crate, all application logic
└── main.rs   # binary entry point
```

## License

[Mozilla Public License 2.0](LICENSE).