#!/usr/bin/env bash
# The @x402/fetch leg of the x402 e2e. Deliberately unpinned: the latest release
# is fetched and its version printed for every run's forensics.
set -euo pipefail
. "$(dirname "$0")/lib.sh"

# One install for all three: a second --no-save install into the same
# node_modules prunes the first one's dependency tree. Installing viem here also
# satisfies the guard in payer_setup.
npm install --no-save --silent @x402/fetch @x402/evm viem
CLIENT_VERSION=$(node -p "JSON.parse(require('fs').readFileSync('node_modules/@x402/fetch/package.json')).version")
echo "@x402/fetch version: $CLIENT_VERSION"

payer_setup

FROM_BLOCK=$(settlement_cursor)
# The .mjs extension is load-bearing: the repo has no package.json declaring
# "type": "module", so a .js file only parses as an ES module on Node 22.7 and
# newer, where syntax detection is on by default.
PAYER_KEY="$PAYER_KEY" URL="$URL" node "$(dirname "$0")/x402-fetch.mjs" 2> client.log
python3 - response.txt <<'EOF'
import json, sys

body = json.load(open(sys.argv[1]))
assert body.get("type") == "search", list(body)[:5]
print("paid request returned the search body")
EOF

verify_settlement "$FROM_BLOCK"
