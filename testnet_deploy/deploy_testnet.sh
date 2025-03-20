#!/bin/bash
set -euo pipefail

# Configuration variables
NODE="https://testnet.sentry.tm.injective.network:443"
CHAIN_ID="injective-888"
FEES="1500000000000000inj"
GAS="3500000"
FROM="testnet"
PASSWORD="12345678"

# Admin address variable. Address of FROM
ADMIN_ADDRESS="inj1q2m26a7jdzjyfdn545vqsude3zwwtfrdap5jgz"
# An address to send the dex fees to
FEE_WALLET_ADDRESS="inj1nwk46lyvhmdj5hr8ynwdvz0jaa4men9ce2gt58"

# Helper function to store a contract.
# Expects one argument: the wasm file path.
# Returns the code id.
store_contract() {
    local wasm_file="$1"
    tx_output=$(yes $PASSWORD | injectived tx wasm store "$wasm_file" \
      --from="$FROM" \
      --chain-id="$CHAIN_ID" \
      --yes --fees="$FEES" --gas="$GAS" \
      --node="$NODE" 2>&1)
    # Extract the tx hash from the output.
    txhash=$(echo "$tx_output" | grep -o 'txhash: [A-F0-9]*' | awk '{print $2}')
    # Wait a moment for the transaction to be indexed.
    sleep 1
    query_output=$(injectived query tx "$txhash" --node="$NODE")
    # Use awk to find the code_id line, strip quotes, and print the first occurrence.
    code_id=$(echo "$query_output" | grep -A 1 'key: code_id' | grep 'value:' | head -1 | sed 's/.*value: "\(.*\)".*/\1/')
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
    # Extract the txhash from the tx output.
    txhash=$(echo "$tx_output" | grep -o 'txhash: [A-F0-9]*' | awk '{print $2}')
    sleep 1
    query_output=$(injectived query tx "$txhash" --node="$NODE")
    # Extract the contract address from the query output.
    contract_address=$(echo "$query_output" \
    | grep -A 1 'key: contract_address' \
    | grep 'value:' \
    | head -1 \
    | sed "s/.*value: //; s/['\"]//g")
    echo "$contract_address"
}

#############################
# Deployment Process
#############################

# 1. Store pair contract 
PAIR_CODE_ID=$(store_contract "../artifacts/choice_pair.wasm")

# 2. Store factory contract 
FACTORY_CODE_ID=$(store_contract "../artifacts/choice_factory.wasm")

# 3. Store burn manager (send_to_auction) contract 
BURN_MANAGER_CODE_ID=$(store_contract "../artifacts/choice_send_to_auction.wasm")

# 4. Store cw20 adapter contract 
CW20_ADAPTER_CODE_ID=$(store_contract "../cw20_adapter/cw20_adapter.wasm")

# 5. Instantiate cw20 adapter
INIT_CW20='{}'
CW20_ADAPTER_ADDRESS=$(instantiate_contract "$CW20_ADAPTER_CODE_ID" "$INIT_CW20" "AdapterContractInstance")

# 6. Instantiate burn manager contract 
INIT_BURN=$(cat <<EOF
{
  "admin": "$ADMIN_ADDRESS",
  "adapter_contract": "$CW20_ADAPTER_ADDRESS",
  "burn_auction_subaccount": "0x1111111111111111111111111111111111111111111111111111111111111111"
}
EOF
)
BURN_MANAGER_ADDRESS=$(instantiate_contract "$BURN_MANAGER_CODE_ID" "$INIT_BURN" "ChoiceSendToBurnAuction" "$ADMIN_ADDRESS")

# 7. Instantiate factory contract 
INIT_FACTORY=$(cat <<EOF
{
  "burn_address": "$BURN_MANAGER_ADDRESS",
  "fee_wallet_address": "$FEE_WALLET_ADDRESS",
  "pair_code_id": $PAIR_CODE_ID
}
EOF
)
FACTORY_ADDRESS=$(instantiate_contract "$FACTORY_CODE_ID" "$INIT_FACTORY" "FactoryContractInstance" "$ADMIN_ADDRESS")

# 8. Store router contract 
ROUTER_CODE_ID=$(store_contract "../artifacts/choice_router.wasm")

# 9. Instantiate router contract 
INIT_ROUTER=$(cat <<EOF
{
  "choice_factory": "$FACTORY_ADDRESS"
}
EOF
)
ROUTER_ADDRESS=$(instantiate_contract "$ROUTER_CODE_ID" "$INIT_ROUTER" "ChoiceRouterInstance" "$ADMIN_ADDRESS")

#############################
# Deployment Summary
#############################

echo "-------------------------------"
echo "Deployment Summary:"
echo ""
echo "Code IDs:"
printf "  %-20s %s\n" "Pair:" "$PAIR_CODE_ID"
printf "  %-20s %s\n" "Factory:" "$FACTORY_CODE_ID"
printf "  %-20s %s\n" "Burn Manager:" "$BURN_MANAGER_CODE_ID"
printf "  %-20s %s\n" "CW20 Adapter:" "$CW20_ADAPTER_CODE_ID"
printf "  %-20s %s\n" "Router:" "$ROUTER_CODE_ID"
echo ""
echo "Contract Addresses:"
printf "  %-20s %s\n" "CW20 Adapter:" "$CW20_ADAPTER_ADDRESS"
printf "  %-20s %s\n" "Burn Manager:" "$BURN_MANAGER_ADDRESS"
printf "  %-20s %s\n" "Factory:" "$FACTORY_ADDRESS"
printf "  %-20s %s\n" "Router:" "$ROUTER_ADDRESS"
echo ""
printf "Fee Wallet Address:        %s\n" "$FEE_WALLET_ADDRESS"
printf "Admin Address:        %s\n" "$ADMIN_ADDRESS"
echo "-------------------------------"