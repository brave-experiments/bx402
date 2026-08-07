#!/usr/bin/env bash
# The purl leg of the x402 e2e. Deliberately unpinned: the latest release is
# fetched and its version printed for every run's forensics.
set -euo pipefail
. "$(dirname "$0")/lib.sh"

case "$(uname -s)-$(uname -m)" in
  Linux-x86_64) PLATFORM=linux-amd64 ;;
  Darwin-arm64) PLATFORM=darwin-arm64 ;;
  *) echo "unsupported platform for purl" >&2; exit 1 ;;
esac
curl -sfL -m 60 -o purl "https://github.com/stripe/purl/releases/latest/download/purl-$PLATFORM"
chmod +x purl
PURL_VERSION=$(./purl --version)
echo "purl version: $PURL_VERSION"

payer_setup
# Without --type purl prompts and dies with "IO error: not a terminal" on a
# runner, and --set-active takes a mandatory value.
./purl wallet add --name e2e --type evm --private-key "$PAYER_KEY" \
  --password e2e-throwaway --set-active true

FROM_BLOCK=$(settlement_cursor)
# The password is needed again here, to decrypt the keystore: without it purl
# prompts and dies the same way. --output-format sets the response encoding,
# where --output would write the response to a file of that name instead.
PURL_PASSWORD=e2e-throwaway ./purl --output-format json "$URL" > response.txt 2> client.log
assert_search_body response.txt
verify_settlement "$FROM_BLOCK"
