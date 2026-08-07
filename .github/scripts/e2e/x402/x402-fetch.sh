#!/usr/bin/env bash
# The @x402/fetch leg of the x402 e2e. Deliberately unpinned: the latest release
# is fetched and its version printed for every run's forensics.
set -euo pipefail
. "$(dirname "$0")/lib.sh"

npm install --no-save --silent @x402/fetch @x402/evm viem
npm_client_version @x402/fetch

payer_setup

FROM_BLOCK=$(settlement_cursor)
run_node_client x402-fetch.mjs
assert_search_body response.txt
verify_settlement "$FROM_BLOCK"
