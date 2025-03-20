#!/bin/bash
set -euo pipefail

# Configuration variables
NODE="https://testnet.sentry.tm.injective.network:443"
CHAIN_ID="injective-888"
# Fees for the WASM execute transactions (this fee is for the execute messages; adjust if necessary)
FEES="1500000000000000inj"
GAS="3000000"
FROM="testnet"
PASSWORD="12345678"

# Factory contract address (update this to your deployed factory contract address)
FACTORY_CONTRACT="inj185n47hcx7lm4art0j8sm2yu4a7pdnpgdpjrcsy"

# Admin address that sends the bank funds (update to admin of the factory contract)
ADMIN_ADDRESS="inj1q2m26a7jdzjyfdn545vqsude3zwwtfrdap5jgz"

# List of tokens and their decimals in the format "denom:decimals"
# Add additional tokens to this list as needed.
TOKENS=(
  "inj:18"                                                      # INJ
  "factory/inj17vytdwqczqz72j65saukplrktd4gyfme5agf6c/usdc:6"   # USDC
  "factory/inj17vytdwqczqz72j65saukplrktd4gyfme5agf6c/wbtc:8"   # wBTC
  "factory/inj17vytdwqczqz72j65saukplrktd4gyfme5agf6c/atom:8"   # ATOM
  "factory/inj17vytdwqczqz72j65saukplrktd4gyfme5agf6c/weth:8"   # wETH
  "peggy0x87aB3B4C8661e07D6372361211B96ed4Dc36B1B5:6"           # USDT
)

for token in "${TOKENS[@]}"; do
  # Split the token string into denom and decimals
  IFS=":" read -r denom decimals <<< "$token"
  echo "-----------------------------------------------------"
  echo "Adding native token decimals for '$denom' with $decimals decimals."

  # Define amounts for the bank send (if required by your process)
  # Here we send a minimal deposit so the factory can register the native token.
  AMOUNT="1${denom}"

  echo "Sending $AMOUNT from $ADMIN_ADDRESS to factory contract ($FACTORY_CONTRACT)..."
  tx_output=$(yes "$PASSWORD" | injectived tx bank send "$ADMIN_ADDRESS" "$FACTORY_CONTRACT" "$AMOUNT" \
    --from="$FROM" \
    --chain-id="$CHAIN_ID" \
    --yes --fees="$FEES" --gas="$GAS" \
    --node="$NODE" 2>&1) || true

  txhash=$(echo "$tx_output" | grep -o 'txhash: [A-F0-9]*' | awk '{print $2}')
  echo tx hash: $txhash
  
  # Pause to allow the transaction to be indexed (adjust sleep duration as needed)
  sleep 2

  # Construct the JSON message for the factory contract execute message
  MSG=$(cat <<EOF
{
  "add_native_token_decimals": {
    "denom": "$denom",
    "decimals": $decimals
  }
}
EOF
)
  echo "Executing factory contract to add token decimals $decimals for '$denom'..."
  tx_output=$(yes "$PASSWORD" | injectived tx wasm execute "$FACTORY_CONTRACT" "$MSG" \
    --from="$FROM" \
    --chain-id="$CHAIN_ID" \
    --yes --fees="$FEES" --gas="$GAS" \
    --node="$NODE" 2>&1) || true
  
  txhash=$(echo "$tx_output" | grep -o 'txhash: [A-F0-9]*' | awk '{print $2}')
  echo tx hash: $txhash
  
  # Wait a bit before processing the next token
  sleep 2
done

echo "-----------------------------------------------------"
echo "All token decimals have been added to the factory contract."
