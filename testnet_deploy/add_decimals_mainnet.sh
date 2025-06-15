#!/bin/bash
set -euo pipefail

# Configuration variables
NODE="https://sentry.tm.injective.network:443"
CHAIN_ID="injective-1"
# Fees for the WASM execute transactions (this fee is for the execute messages; adjust if necessary)
FEES="1500000000000000inj"
GAS="3000000"
FROM="testnet"
PASSWORD="12345678"

# Factory contract address (update this to your deployed factory contract address)
FACTORY_CONTRACT="inj1k9lcqtn3y92h4t3tdsu7z8qx292mhxhgsssmxg"

# Admin address that sends the bank funds (update to admin of the factory contract)
ADMIN_ADDRESS="inj1q2m26a7jdzjyfdn545vqsude3zwwtfrdap5jgz"

# List of tokens and their decimals in the format "denom:decimals"
# Add additional tokens to this list as needed.
TOKENS=(
  "inj:18"                                               # INJ
  "peggy0x36E66fbBce51e4cD5bd3C62B637Eb411b18949D4:18"   # OMNI 
  "peggy0x4c9EDD5852cd905f086C759E8383e09bff1E68B3:18"   # USDe
  "peggy0xb2617246d0c6c0087f18703d576831899ca94f01:18"   # ZIG
  "peggy0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2:18"   # WETH
  "peggy0xdAC17F958D2ee523a2206206994597C13D831ec7:6"    # USDT
  "ibc/F51BB221BAA275F2EBF654F70B005627D7E713AFFD6D86AFD1E43CAA886149F4:6"    # TIA
  "ibc/F3330C1B8BD1886FE9509B94C7B5398B892EA41420D2BC0B7C6A53CB8ED761D6:6"    # PYTH
  "ibc/D6E6A20ABDD600742D22464340A7701558027759CE14D12590F8EA869CCCF445:6"    # WHALE
  "ibc/C4CFF46FD6DE35CA4CF4CE031E643C8FDC9BA4B99AE598E9B0ED98FE3A2319F9:6"    # ATOM
  "ibc/AF921F0874131B56897A11AA3F33D5B29CD9C147A1D7C37FE8D918CB420956B2:6"    # SAGA
  "ibc/AC87717EA002B0123B10A05063E69BCA274BA2C44D842AEEB41558D2856DCE93:18"   # STINJ
  "ibc/A8B0B746B5AB736C2D8577259B510D56B8AF598008F68041E3D634BCDE72BE97:8"    # SOL
  "ibc/57AA1A70A4BC9769C525EBF6386F7A21536E04A79D62E1981EFCEF9428EBB205:6"    # KAVA
  "ibc/2CBC2EA121AE42563B08028466F37B600F2D7D4282342DE938283CC3FB2BC00E:6"    # USDC
  "ibc/00BF66BAB34873B07FB9EEEBCFACEA11FB4BB348718862AA7782D6DECC1F44C8:6"    # XION 
  "factory/inj10aa0h5s0xwzv95a8pjhwluxcm5feeqygdk3lkm/SAI:18"     # SAI
  "factory/inj127l5a2wmkyvucxdlupqyac3y0v6wqfhq03ka64/qunt:6"     # QUNT
  "factory/inj14lf8xm6fcvlggpa7guxzjqwjmtr24gnvf56hvz/autism:6"   # AUTISM
  "factory/inj16dd5xzszud3u5wqphr3tq8eaz00gjdn3d4mvj8/agent:6"    # AGENT
  "factory/inj16eckaf75gcu9uxdglyvmh63k9t0l7chd0qmu85/black:6"    # BLACK
  "factory/inj172ccd0gddgz203e4pf86ype7zjx573tn8g0df9/GINGER:6"   # GINGER
  "factory/inj178zy7myyxewek7ka7v9hru8ycpvfnen6xeps89/DRUGS:6"    # DRUGS
  "factory/inj18flmwwaxxqj8m8l5zl8xhjrnah98fcjp3gcy3e/XIII:6"     # XIII
  "factory/inj18xsczx27lanjt40y9v79q0v57d76j2s8ctj85x/POOR:6"     # POOR
  "factory/inj1a6xdezq7a94qwamec6n6cnup02nvewvjtz6h6e/SYN:6"      # SYN
  "factory/inj1dxp690rd86xltejgfq2fa7f2nxtgmm5cer3hvu/bINJ:18"    # bINJ
  "factory/inj1etz0laas6h7vemg3qtd67jpr6lh8v7xz7gfzqw/hdro:6"     # HDRO
  "factory/inj1fefpnm6pklz3av6zzjmr5z070779a9m4sx384v/jni:18"     # JNI
  "factory/inj1llr45x92t7jrqtxvc02gpkcqhqr82dvyzkr4mz/NBZ:6"      # NBZ
  "factory/inj1n636d9gzrqggdk66n2f97th0x8yuhfrtx520e7/ausd:6"     # AUSD
  "factory/inj1nw35hnkz5j74kyrfq9ejlh2u4f7y7gt7c3ckde/PUGGO:18"   # PUGGO
  "factory/inj1v3a4zznudwpukpr8y987pu5gnh4xuf7v36jhva/nept:6"     # NEPT
  "factory/inj1xtel2knkt8hmc9dnzpjz6kdmacgcfmlv5f308w/ninja:6"    # NINJA
  "factory/inj1xy3kvlr4q4wdd6lrelsrw2fk2ged0any44hhwq/KIRA:6"     # KIRA
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
  sleep 5

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
