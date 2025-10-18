#!/bin/bash
set -uo pipefail

# -- Blockchain Details --
NODE="https://testnet.sentry.tm.injective.network:443"
CHAIN_ID="injective-888"
FEES="1500000000000000inj"
GAS="3700000"

# -- Keystore Details --
FROM="testnet"
PASSWORD="12345678"


# -- Contract Details --
WASM_FILE="./artifacts/choice_farm.wasm"
CONTRACT_LABEL="Choice Farm v1.0"

echo " "
echo "================================================="
echo "  🚀 STARTING DEX AGGREGATOR CODE UPLOAD 🚀"
echo "================================================="
echo " "
echo "  Chain ID:      $CHAIN_ID"
echo "  Node:          $NODE"
echo "  Signer:        $FROM"
echo " "

echo "-------------------------------------------------"
echo "  Storing Wasm code..."
echo "-------------------------------------------------"

store_response=$(yes $PASSWORD | injectived tx wasm store "$WASM_FILE" \
  --from="$FROM" \
  --chain-id="$CHAIN_ID" \
  --yes --fees="$FEES" --gas="$GAS" \
  --node="$NODE")

if ! echo "$store_response" | grep -q "txhash"; then
    echo "  ❌ ERROR: Failed to submit store transaction."
    echo "  > Response from injectived:"
    echo "$store_response"
    exit 1
fi

store_txhash=$(echo "$store_response" | grep 'txhash:' | awk '{print $2}')
echo "  > Store transaction submitted: $store_txhash"

echo "  > Waiting for transaction to be indexed..."
sleep 8 # Increased wait time for more reliability

echo "  > Querying transaction for Code ID..."
store_query_output=$(injectived query tx "$store_txhash" --node="$NODE")

CODE_ID=$(echo "$store_query_output" | grep -A 1 'key: code_id' | grep 'value:' | head -1 | sed 's/.*value: "\(.*\)".*/\1/')

if [ -z "$CODE_ID" ]; then
    echo "  ❌ ERROR: Could not find Code ID in transaction logs for tx: $store_txhash"
    echo "  > Please check the transaction on the explorer."
    exit 1
fi
echo "  ✅ Code stored successfully. Code ID: $CODE_ID"
echo " "
