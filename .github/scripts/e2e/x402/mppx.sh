#!/usr/bin/env bash
# The mppx leg of the x402 e2e. Deliberately unpinned: the latest release is
# fetched and its version printed for every run's forensics.
set -euo pipefail
. "$(dirname "$0")/lib.sh"

npm install --no-save --silent mppx viem
npm_client_version mppx

payer_setup

FROM_BLOCK=$(settlement_cursor)
run_node_client mppx.mjs
assert_search_body response.txt
verify_settlement "$FROM_BLOCK"
