#!/usr/bin/env bash
# The mppx leg of the x402 e2e. Deliberately unpinned: the latest release is
# fetched and its version printed for every run's forensics.
set -euo pipefail
. "$(dirname "$0")/lib.sh"

# One install for both: a second --no-save install into the same node_modules
# prunes the first one's dependency tree. Installing viem here also satisfies the
# guard in payer_setup.
npm install --no-save --silent mppx viem
# Read from the file rather than require('mppx/package.json'), which the package
# does not export.
CLIENT_VERSION=$(node -p "JSON.parse(require('fs').readFileSync('node_modules/mppx/package.json')).version")
echo "mppx version: $CLIENT_VERSION"

payer_setup

FROM_BLOCK=$(settlement_cursor)
PAYER_KEY="$PAYER_KEY" URL="$URL" node "$(dirname "$0")/mppx.mjs" 2> client.log
python3 - response.txt <<'EOF'
import json, sys

body = json.load(open(sys.argv[1]))
assert body.get("type") == "search", list(body)[:5]
print("paid request returned the search body")
EOF

verify_settlement "$FROM_BLOCK"
