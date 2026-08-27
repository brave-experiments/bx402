#!/usr/bin/env bash
# The tempo request leg of the MPP e2e: install the Tempo CLI, fund a
# throwaway key, pay for one search, then prove the settlement.
#
# `tempo request` signs with TEMPO_PRIVATE_KEY and follows the server's
# challenge for both chain and token, so the 42431 chain id routes it to
# Moderato and pathUSD without any flags. Deliberately unpinned: the launcher
# chooses both its own version and the request extension's, and prints its
# version for every run's forensics.
set -euo pipefail
. "$(dirname "$0")/lib.sh"

# The launcher is the only supported install: releases stopped carrying
# standalone tempo-request binaries after v0.7.0. It manages the request
# extension itself, checks a sha256 checksum, and verifies the release build
# attestation when GH_TOKEN gives it an authenticated gh. Without that token it
# falls back to a GPG path whose key fetch fails, so the token is required.
curl -fsSL -m 60 https://tempo.xyz/install | bash
. "$HOME/.tempo/env"
TEMPO_VERSION=$(tempo --version | head -1)
echo "tempo version: $TEMPO_VERSION"

new_payer
export TEMPO_PRIVATE_KEY="$PAYER_KEY"

tempo request -i "$URL" > response.txt 2> client.log
assert_http_200 response.txt

verify_settlement response.txt
