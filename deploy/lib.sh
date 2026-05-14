# Shared helpers for deploy/ scripts. Source from each script:
#
#     source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"
#
# After sourcing, the following are available:
#   $NETWORK / $NODE / $CHAIN_ID / $GAS / $FEES / $FROM / $PASSWORD /
#   $SIGNER_ADDRESS — from network/${NETWORK}.env (default: testnet).
#   wait_for_tx, store_contract, instantiate_contract — helpers.
#
# The caller can override NETWORK via env:
#     NETWORK=mainnet ./upload_farm_code.sh
#
# Each helper writes status to stderr and the captured value (code id or
# contract address) to stdout, so $() capture works without slurping noise.

# Resolve the deploy/ directory regardless of which subshell is sourcing us.
DEPLOY_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

NETWORK="${NETWORK:-testnet}"
NET_ENV="${DEPLOY_DIR}/network/${NETWORK}.env"
if [ ! -f "$NET_ENV" ]; then
    echo "ERROR: unknown NETWORK '$NETWORK' (no $NET_ENV)" >&2
    exit 1
fi
# shellcheck source=/dev/null
source "$NET_ENV"

# Sanity — bail if a required field is missing or empty.
for var in NODE CHAIN_ID GAS FEES FROM PASSWORD SIGNER_ADDRESS; do
    if [ -z "${!var:-}" ]; then
        echo "ERROR: $var not set in $NET_ENV" >&2
        exit 1
    fi
done

# Polls injectived for the tx's indexed record. Exits non-zero on tx code != 0
# (i.e. on-chain failure) or if not indexed after $max_attempts. Echoes the
# full raw query output to stdout on success — caller can re-parse it.
wait_for_tx() {
    local txhash="$1"
    local max_attempts="${2:-10}"
    echo "  Waiting for tx $txhash ..." >&2
    local i query_output tx_code raw_log
    for i in $(seq 1 "$max_attempts"); do
        if query_output=$(injectived query tx "$txhash" --node="$NODE" 2>/dev/null); then
            tx_code=$(echo "$query_output" | grep '^code:' | head -1 | awk '{print $2}')
            if [ "$tx_code" != "0" ]; then
                raw_log=$(echo "$query_output" | grep '^raw_log:' | head -1 | sed "s/raw_log: '//; s/'$//")
                echo "ERROR: tx $txhash failed (code=$tx_code): $raw_log" >&2
                return 1
            fi
            echo "$query_output"
            return 0
        fi
        echo "  Attempt $i/$max_attempts — not indexed yet, retrying..." >&2
        sleep 3
    done
    echo "ERROR: tx $txhash not found after $max_attempts attempts" >&2
    return 1
}

# Store a wasm. Echoes the resulting code id to stdout on success.
store_contract() {
    local wasm_path="$1"
    if [ ! -f "$wasm_path" ]; then
        echo "ERROR: wasm not found at $wasm_path. Run ./build_release.sh first." >&2
        return 1
    fi
    local tx_output txhash query_output code_id
    tx_output=$(printf '%s\n' "$PASSWORD" | injectived tx wasm store "$wasm_path" \
        --from="$FROM" \
        --chain-id="$CHAIN_ID" \
        --yes --fees="$FEES" --gas="$GAS" \
        --node="$NODE" 2>&1)
    txhash=$(echo "$tx_output" | grep -o 'txhash: [A-F0-9]*' | awk '{print $2}')
    if [ -z "$txhash" ]; then
        echo "ERROR: could not parse txhash from store output" >&2
        echo "$tx_output" >&2
        return 1
    fi
    echo "  > store tx: $txhash" >&2
    query_output=$(wait_for_tx "$txhash") || return 1
    code_id=$(echo "$query_output" | grep -A 1 'key: code_id' | grep 'value:' | head -1 | sed 's/.*value: "\(.*\)".*/\1/')
    if [ -z "$code_id" ]; then
        echo "ERROR: could not extract code id from tx $txhash" >&2
        return 1
    fi
    echo "$code_id"
}

# Instantiate a contract. Echoes the new contract address to stdout on success.
# Arguments: code_id, init_msg_json, label, [admin: bech32 | "" for --no-admin]
instantiate_contract() {
    local code_id="$1"
    local init_msg="$2"
    local label="$3"
    local admin="${4:-$SIGNER_ADDRESS}"
    local tx_output txhash query_output addr admin_flag
    if [ -z "$admin" ]; then
        admin_flag="--no-admin"
    else
        admin_flag="--admin=$admin"
    fi
    tx_output=$(printf '%s\n' "$PASSWORD" | injectived tx wasm instantiate "$code_id" "$init_msg" \
        --label="$label" \
        $admin_flag \
        --from="$FROM" \
        --chain-id="$CHAIN_ID" \
        --yes --fees="$FEES" --gas="$GAS" \
        --node="$NODE" 2>&1)
    txhash=$(echo "$tx_output" | grep -o 'txhash: [A-F0-9]*' | awk '{print $2}')
    if [ -z "$txhash" ]; then
        echo "ERROR: could not parse txhash from instantiate output" >&2
        echo "$tx_output" >&2
        return 1
    fi
    echo "  > instantiate tx: $txhash" >&2
    query_output=$(wait_for_tx "$txhash") || return 1
    # Newer injectived emits `_contract_address`, older versions emit
    # `contract_address`. Try both.
    addr=$(echo "$query_output" \
        | grep -A 1 'key: _contract_address' \
        | grep 'value:' \
        | head -1 \
        | sed "s/.*value: //; s/['\"]//g")
    if [ -z "$addr" ]; then
        addr=$(echo "$query_output" \
            | grep -A 1 'key: contract_address' \
            | grep 'value:' \
            | head -1 \
            | sed "s/.*value: //; s/['\"]//g")
    fi
    if [ -z "$addr" ]; then
        echo "ERROR: could not extract contract address from tx $txhash" >&2
        return 1
    fi
    echo "$addr"
}

# Pretty banner. Cheap chrome that matches the legacy scripts' style.
banner() {
    echo " "
    echo "================================================="
    echo "  $1"
    echo "================================================="
    echo "  Network:   $NETWORK ($CHAIN_ID)"
    echo "  Node:      $NODE"
    echo "  Signer:    $FROM ($SIGNER_ADDRESS)"
    echo " "
}
