#!/usr/bin/env bash
# The tempo request leg of the MPP e2e: fetch the Tempo wallet CLI's standalone
# binary, fund a throwaway key, pay for one search, then prove the settlement.
#
# `tempo request` signs with TEMPO_PRIVATE_KEY and follows the server's
# challenge for both chain and token, so the 42431 chain id routes it to
# Moderato and pathUSD without any flags. Deliberately unpinned: the latest
# release is fetched and its version printed for every run's forensics.
set -euo pipefail
. "$(dirname "$0")/lib.sh"

case "$(uname -s)-$(uname -m)" in
  Linux-x86_64) PLATFORM=linux-amd64 ;;
  Linux-aarch64) PLATFORM=linux-arm64 ;;
  Darwin-arm64) PLATFORM=darwin-arm64 ;;
  Darwin-x86_64) PLATFORM=darwin-amd64 ;;
  *) echo "unsupported platform for tempo-request" >&2; exit 1 ;;
esac
RELEASE="https://github.com/tempoxyz/wallet-cli/releases/latest/download"
curl -sfL -m 60 -o tempo-request "$RELEASE/tempo-request-$PLATFORM"
curl -sfL -m 30 -o tempo-request.sha256 "$RELEASE/tempo-request-$PLATFORM.sha256"
# The published checksum names the release's artifacts directory, so it is
# rewritten for the local file name.
echo "$(cut -d' ' -f1 tempo-request.sha256)  tempo-request" | shasum -a 256 -c > /dev/null
chmod +x tempo-request
TEMPO_REQUEST_VERSION=$(./tempo-request --version)
echo "tempo-request version: $TEMPO_REQUEST_VERSION"

new_payer
export TEMPO_PRIVATE_KEY="$PAYER_KEY"

./tempo-request -i "$URL" > response.txt 2> client.log
assert_http_200 response.txt

verify_settlement response.txt
