#!/usr/bin/env bash
# The mppx leg of the MPP e2e: create a throwaway key, fund it from the
# Moderato faucet, pay for one search, then prove the settlement on chain.
#
# mppx signs with MPPX_PRIVATE_KEY when it is set, so no account store or OS
# keyring is involved. The faucet is called directly over RPC because
# `mppx account fund` only funds stored accounts.
set -euo pipefail
. "$(dirname "$0")/lib.sh"

# Installed into the working directory. Deliberately unpinned: this e2e exists
# to prove interop with the client users actually install today, so the version
# floats and is printed for every run's forensics.
npm install --no-save --silent mppx
MPPX_VERSION=$(./node_modules/.bin/mppx --version)
echo "mppx version: $MPPX_VERSION"

new_payer
export MPPX_PRIVATE_KEY="$PAYER_KEY"

./node_modules/.bin/mppx --network testnet -i "$URL" > response.txt 2> client.log
grep -Eq "^HTTP/[0-9.]+ 200" response.txt
echo "paid request returned 200"

verify_settlement response.txt
