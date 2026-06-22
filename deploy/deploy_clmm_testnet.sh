#!/bin/bash
set -euo pipefail

# Configuration variables (env-overridable)
NODE="${NODE:-https://testnet.sentry.tm.injective.network:443}"
CHAIN_ID="${CHAIN_ID:-injective-888}"
GAS="${GAS:-8000000}"
FEES="${FEES:-4000000000000000inj}"
FROM="${FROM:-testnet}"
PASSWORD="${PASSWORD:?export PASSWORD (keyring passphrase) — never commit it}"

# Admin address (same as FROM)
ADMIN_ADDRESS="inj1q2m26a7jdzjyfdn545vqsude3zwwtfrdap5jgz"

# Helper function to store a contract.
# Expects one argument: the wasm file path.
# Returns the code id.
wait_for_tx() {
    local txhash="$1"
    local max_attempts=10
    echo "  Waiting for tx $txhash ..." >&2
    for i in $(seq 1 $max_attempts); do
        if query_output=$(injectived query tx "$txhash" --node="$NODE" 2>/dev/null); then
            # Query succeeded — check for tx failure
            tx_code=$(echo "$query_output" | grep '^code:' | head -1 | awk '{print $2}')
            if [ "$tx_code" != "0" ]; then
                raw_log=$(echo "$query_output" | grep '^raw_log:' | head -1 | sed "s/raw_log: '//; s/'$//")
                echo "ERROR: Transaction failed with code $tx_code: $raw_log" >&2
                return 1
            fi
            echo "$query_output"
            return 0
        fi
        echo "  Attempt $i/$max_attempts — not indexed yet, retrying..." >&2
        sleep 3
    done
    echo "ERROR: Transaction $txhash not found after $max_attempts attempts" >&2
    return 1
}

store_contract() {
    local wasm_file="$1"
    tx_output=$(yes $PASSWORD | injectived tx wasm store "$wasm_file" \
      --from="$FROM" \
      --chain-id="$CHAIN_ID" \
      --yes --fees="$FEES" --gas="$GAS" \
      --node="$NODE" 2>&1)
    txhash=$(echo "$tx_output" | grep -o 'txhash: [A-F0-9]*' | awk '{print $2}')
    if [ -z "$txhash" ]; then
        echo "ERROR: Failed to get txhash from store output" >&2
        return 1
    fi
    query_output=$(wait_for_tx "$txhash") || return 1
    code_id=$(echo "$query_output" | grep -A 1 'key: code_id' | grep 'value:' | head -1 | sed 's/.*value: "\(.*\)".*/\1/')
    if [ -z "$code_id" ]; then
        echo "ERROR: Failed to extract code_id from tx $txhash" >&2
        return 1
    fi
    echo "$code_id"
}

# Helper function to instantiate a contract.
# Arguments: code_id, init message (as JSON string), label, [optional admin]
# Returns the instantiated contract address.
instantiate_contract() {
    local code_id="$1"
    local init_msg="$2"
    local label="$3"
    local admin="${4:-}"
    if [ -z "$admin" ]; then
        tx_output=$(yes $PASSWORD | injectived tx wasm instantiate "$code_id" "$init_msg" \
          --label="$label" \
          --no-admin \
          --from="$FROM" \
          --chain-id="$CHAIN_ID" \
          --yes --fees="$FEES" --gas="$GAS" \
          --node="$NODE" 2>&1)
    else
        tx_output=$(yes $PASSWORD | injectived tx wasm instantiate "$code_id" "$init_msg" \
          --label="$label" \
          --admin="$admin" \
          --from="$FROM" \
          --chain-id="$CHAIN_ID" \
          --yes --fees="$FEES" --gas="$GAS" \
          --node="$NODE" 2>&1)
    fi
    txhash=$(echo "$tx_output" | grep -o 'txhash: [A-F0-9]*' | awk '{print $2}')
    if [ -z "$txhash" ]; then
        echo "ERROR: Failed to get txhash from instantiate output" >&2
        return 1
    fi
    query_output=$(wait_for_tx "$txhash") || return 1
    contract_address=$(echo "$query_output" \
    | grep -A 1 'key: contract_address' \
    | grep 'value:' \
    | head -1 \
    | sed "s/.*value: //; s/['\"]//g")
    if [ -z "$contract_address" ]; then
        echo "ERROR: Failed to extract contract_address from tx $txhash" >&2
        return 1
    fi
    echo "$contract_address"
}

#############################
# CLMM Deployment Process
#############################

echo "=== Deploying CLMM Contracts ==="
echo ""

# 1. Store CLMM pool contract
echo "Storing CLMM pool contract..."
POOL_CODE_ID=$(store_contract "../artifacts/choice_clmm_pool.wasm")
echo "  Pool code ID: $POOL_CODE_ID"

# 2. Store CLMM factory contract
echo "Storing CLMM factory contract..."
FACTORY_CODE_ID=$(store_contract "../artifacts/choice_clmm_factory.wasm")
echo "  Factory code ID: $FACTORY_CODE_ID"

# 3. Store CLMM manager contract
echo "Storing CLMM manager contract..."
MANAGER_CODE_ID=$(store_contract "../artifacts/choice_clmm_manager.wasm")
echo "  Manager code ID: $MANAGER_CODE_ID"

# 4. Instantiate CLMM factory (needs pool code ID to create pools)
echo "Instantiating CLMM factory..."
INIT_FACTORY=$(cat <<EOF
{
  "pool_code_id": $POOL_CODE_ID
}
EOF
)
FACTORY_ADDRESS=$(instantiate_contract "$FACTORY_CODE_ID" "$INIT_FACTORY" "ChoiceCLMMFactory" "$ADMIN_ADDRESS")
echo "  Factory address: $FACTORY_ADDRESS"

# 5. Instantiate CLMM manager (NFT position manager)
echo "Instantiating CLMM manager..."
INIT_MANAGER=$(cat <<EOF
{
  "name": "Choice CL Positions",
  "symbol": "CHOICE-CLP",
  "factory_addr": "$FACTORY_ADDRESS"
}
EOF
)
MANAGER_ADDRESS=$(instantiate_contract "$MANAGER_CODE_ID" "$INIT_MANAGER" "ChoiceCLMMManager" "$ADMIN_ADDRESS")
echo "  Manager address: $MANAGER_ADDRESS"

#############################
# Deployment Summary
#############################

echo ""
echo "-------------------------------"
echo "CLMM Deployment Summary:"
echo ""
echo "Code IDs:"
printf "  %-20s %s\n" "Pool:" "$POOL_CODE_ID"
printf "  %-20s %s\n" "Factory:" "$FACTORY_CODE_ID"
printf "  %-20s %s\n" "Manager:" "$MANAGER_CODE_ID"
echo ""
echo "Contract Addresses:"
printf "  %-20s %s\n" "Factory:" "$FACTORY_ADDRESS"
printf "  %-20s %s\n" "Manager:" "$MANAGER_ADDRESS"
echo ""
echo "Add to your .env:"
echo "  VITE_CLMM_FACTORY_ADDRESS=$FACTORY_ADDRESS"
echo "  VITE_CLMM_MANAGER_ADDRESS=$MANAGER_ADDRESS"
echo ""
printf "Admin Address:        %s\n" "$ADMIN_ADDRESS"
echo "-------------------------------"
