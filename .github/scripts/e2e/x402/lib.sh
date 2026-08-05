# Shared plumbing for the x402 e2e client scripts. Source this, then call:
#
#   payer_setup                  reads the persistent payer key from
#                                X402_PAYER_PRIVATE_KEY into PAYER_KEY and
#                                PAYER_ADDR and asserts it can cover the price
#   settlement_cursor            the block to search from, captured before paying
#   verify_settlement <block>    finds the USDC transfer that paid for the search
#                                and prints its transaction hash
#
# Base Sepolia has no faucet RPC, so the payer is a persistent funded wallet
# rather than a throwaway. Sharing it across parallel legs is safe: EIP-3009
# authorizations use random nonces and the facilitator pays the gas.

RPC="${BASE_SEPOLIA_RPC_URL:-https://sepolia.base.org}"
URL="http://localhost:8080/res/v1/web/search?q=base+sepolia"
USDC="0x036CbD53842c5426634e7929541eC2318f3dCF7e"
PRICE=5000
# Mirrors PAY_TO_EVM in src/x402.rs.
TREASURY="0xbd9420A98a7Bd6B89765e5715e169481602D9c3d"
# keccak("Transfer(address,address,uint256)"); both addresses are indexed, so the
# amount is the log's data.
TRANSFER_TOPIC="0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef"

rpc() {
  curl -sf -m 15 -X POST "$RPC" -H 'content-type: application/json' \
    -d "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"$1\",\"params\":$2}"
}

payer_setup() {
  # A second --no-save install into the same node_modules prunes the first's tree.
  node -e 'require.resolve("viem/accounts")' 2> /dev/null \
    || npm install --no-save --silent viem
  PAYER_KEY="${X402_PAYER_PRIVATE_KEY:?set X402_PAYER_PRIVATE_KEY to the funded payer key}"
  PAYER_ADDR=$(PAYER_KEY="$PAYER_KEY" node -e \
    'console.log(require("viem/accounts").privateKeyToAccount(process.env.PAYER_KEY).address)')

  local balance
  balance=$(($(rpc eth_call "[{\"to\":\"$USDC\",\"data\":\"0x70a08231000000000000000000000000${PAYER_ADDR#0x}\"},\"latest\"]" | jq -re .result)))
  echo "payer: $PAYER_ADDR ($balance base units of USDC)"
  [ "$balance" -ge "$PRICE" ] || {
    echo "payer cannot cover the $PRICE price, refill it at faucet.circle.com" >&2
    return 1
  }
}

topic_addr() {
  printf '0x%064s' "${1#0x}" | tr ' ' '0' | tr '[:upper:]' '[:lower:]'
}

settlement_cursor() {
  rpc eth_blockNumber '[]' | jq -re .result
}

# purl never surfaces the PAYMENT-RESPONSE header, so the transaction is recovered
# from the chain. Matching payer, treasury and exact amount together keeps legs
# that share the payer wallet from satisfying each other's filter.
verify_settlement() {
  local from_block="$1"
  local want tx filter
  want=$(printf '0x%064x' "$PRICE")
  filter="{\"address\":\"$USDC\",\"fromBlock\":\"$from_block\",\"toBlock\":\"latest\",\"topics\":[\"$TRANSFER_TOPIC\",\"$(topic_addr "$PAYER_ADDR")\",\"$(topic_addr "$TREASURY")\"]}"
  for _ in $(seq 1 30); do
    tx=$(rpc eth_getLogs "[$filter]" \
      | jq -r --arg want "$want" 'first(.result[] | select(.data == $want) | .transactionHash) // ""') || tx=""
    if [ -n "$tx" ]; then
      echo "settled on chain: $tx"
      echo "  https://sepolia.basescan.org/tx/$tx"
      return 0
    fi
    sleep 2
  done
  echo "no USDC transfer of $PRICE base units from $PAYER_ADDR to $TREASURY since block $((from_block))" >&2
  return 1
}
