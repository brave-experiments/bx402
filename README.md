# bx402

[![CI](https://github.com/brave-experiments/bx402/actions/workflows/ci.yml/badge.svg)](https://github.com/brave-experiments/bx402/actions/workflows/ci.yml)
[![made-with-rust](https://img.shields.io/badge/Made%20with-Rust-1f425f.svg)](https://www.rust-lang.org/)

A pay-per-request proxy in front of the [Brave Search API](https://brave.com/search/api/).
Instead of an API key, each request carries a stablecoin micropayment, making the signed payment
act as the credential.

`bx402` is `bx` ([Brave Search CLI](https://github.com/brave/brave-search-cli)) + `402`, the HTTP *Payment Required* status.
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

You need [`rustup`](https://rustup.rs/) (the pinned toolchain installs on first
build) and a Brave Search API key (free tier at
[brave.com/search/api](https://brave.com/search/api)).

## Paying for a search on Base Sepolia

This runs `bx402` in Docker with a self-hosted
[x402 facilitator](https://github.com/x402-rs/x402-rs) sidecar that settles the
payment on chain.

| Party              | Owner | Requires               | Faucet                                                  |
| ------------------ | ----- | ---------------------- | ------------------------------------------------------- |
| Facilitator signer | Brave | Base Sepolia ETH (gas) | [Alchemy](https://www.alchemy.com/faucets/base-sepolia) |
| Payer wallet       | Agent | Base Sepolia USDC      | [Circle](https://faucet.circle.com/)                    |
| Treasury address   | Brave | nothing                | —                                                       |

1. Clone, then write your Brave Search API key and a throwaway facilitator key to `.env`:
   ```sh
   git clone git@github.com:brave-experiments/bx402.git
   cd bx402
   echo "BRAVE_SEARCH_API_KEY=<your-key>" >> .env
   echo "X402_FACILITATOR_PRIVATE_KEY=0x$(openssl rand -hex 32)" >> .env
   ```
2. Start the stack and note the signer address to fund with ETH on Base Sepolia:
   ```sh
   docker compose up --build -d
   docker compose logs facilitator | grep signers
   # Using EVM provider chain=eip155:84532 signers=[0xebd9…fb45]
   ```
3. Create a payer wallet and fund it with USDC on Base Sepolia (`brew install stripe/purl/purl` if needed):
   ```sh
   purl wallet add --type evm
   purl balance --network base-sepolia
   ```
4. Pay for a search:
   ```sh
   purl inspect 'http://localhost:8080/res/v1/web/search?q=rust'
   purl -v --max-amount 10000 'http://localhost:8080/res/v1/web/search?q=rust'
   ```
   The server returns the settled tx hash in the `PAYMENT-RESPONSE` response
   header, but purl does not print it. Read it from the facilitator logs instead,
   then look it up on [sepolia.basescan.org](https://sepolia.basescan.org).
