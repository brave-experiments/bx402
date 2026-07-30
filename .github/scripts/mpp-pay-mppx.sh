#!/usr/bin/env bash
# The mppx leg of the MPP e2e: create a throwaway key, fund it from the
# Moderato faucet, pay for one search, then prove the settlement on chain.
#
# mppx signs with MPPX_PRIVATE_KEY when it is set, so no account store or OS
# keyring is involved. The faucet is called directly over RPC because
# `mppx account fund` only funds stored accounts.
set -euo pipefail

RPC="${MPP_RPC_URL:-https://rpc.moderato.tempo.xyz}"
URL="http://localhost:8080/res/v1/web/search?q=tempo+moderato"
# The pathUSD precompile, the same address the mpp crate pins as PATH_USD.
PATH_USD="0x20c0000000000000000000000000000000000000"

# A JSON-RPC call against the chain: rpc <method> <params-json>
rpc() {
  curl -sf -m 15 -X POST "$RPC" -H 'content-type: application/json' \
    -d "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"$1\",\"params\":$2}"
}

# Installed into the working directory. Deliberately unpinned: this e2e exists
# to prove interop with the client users actually install today, so the version
# floats and is printed for every run's forensics.
npm install --no-save --silent viem mppx
echo "mppx version: $(./node_modules/.bin/mppx --version)"

export MPPX_PRIVATE_KEY="0x$(openssl rand -hex 32)"
ADDR=$(node -e 'console.log(require("viem/accounts").privateKeyToAccount(process.env.MPPX_PRIVATE_KEY).address)')
echo "payer: $ADDR"

rpc tempo_fundAddress "[\"$ADDR\"]" > /dev/null

BAL=0
for _ in $(seq 1 30); do
  BAL=$(($(rpc eth_call "[{\"to\":\"$PATH_USD\",\"data\":\"0x70a08231000000000000000000000000${ADDR#0x}\"},\"latest\"]" | jq -re .result)))
  if [ "$BAL" -gt 0 ]; then echo "funded: $BAL base units"; break; fi
  sleep 2
done
[ "$BAL" -gt 0 ] || { echo "faucet never funded $ADDR" >&2; exit 1; }

./node_modules/.bin/mppx --network testnet -i "$URL" > response.txt 2> client.log
grep -Eq "^HTTP/[0-9.]+ 200" response.txt
grep -qi "^payment-receipt:" response.txt
echo "paid request returned 200 with a receipt"

TX=$(python3 - response.txt <<'EOF'
import base64, json, re, sys

text = open(sys.argv[1]).read()
header = re.search(r"(?im)^payment-receipt:\s*(\S+)", text).group(1)
receipt = json.loads(base64.b64decode(header + "=" * (-len(header) % 4)))
assert receipt["status"] == "success", receipt
print(receipt["reference"])
EOF
)

BLOCK=$(rpc eth_getTransactionReceipt "[\"$TX\"]" | jq -re 'select(.result.status == "0x1") | .result.blockNumber')
echo "settled on chain: $TX in block $((BLOCK))"
