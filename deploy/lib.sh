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

# Sanity — bail if a required field is missing or empty. PASSWORD is special:
# it is intentionally NOT stored in the env files (public repo) — export the
# keyring passphrase in your shell before running any deploy script.
for var in NODE CHAIN_ID GAS FEES FROM PASSWORD SIGNER_ADDRESS; do
    if [ -z "${!var:-}" ]; then
        if [ "$var" = "PASSWORD" ]; then
            echo "ERROR: PASSWORD not set — export the keyring passphrase in your shell" >&2
        else
            echo "ERROR: $var not set in $NET_ENV" >&2
        fi
        exit 1
    fi
done

# Optional split-signer for Phase 1 stores. If the env file sets STORE_FROM /
# STORE_PASSWORD, `store_contract` uses those instead of FROM / PASSWORD. All
# other helpers (instantiate_contract, exec_contract, set_admin) continue to
# use FROM / PASSWORD. Useful on mainnet to attribute wasm uploads to a
# dedicated dev key while keeping governance txs on the EOA.
STORE_FROM="${STORE_FROM:-$FROM}"
STORE_PASSWORD="${STORE_PASSWORD:-$PASSWORD}"

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
    tx_output=$(printf '%s\n' "$STORE_PASSWORD" | injectived tx wasm store "$wasm_path" \
        --from="$STORE_FROM" \
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

# Execute a contract msg as $FROM and wait for inclusion. Echoes the txhash on
# stdout. rc is captured explicitly: under `set -e` a bare `$(...)` whose
# subshell fails would kill the script silently before we can surface the
# underlying stderr.
exec_contract() {
    local addr="$1"; local msg="$2"; local desc="$3"
    echo "  > exec [$desc] on $addr" >&2
    local tx_output rc=0 txhash
    tx_output=$(printf '%s\n' "$PASSWORD" | injectived tx wasm execute "$addr" "$msg" \
        --from="$FROM" --chain-id="$CHAIN_ID" --yes \
        --fees="$FEES" --gas="$GAS" --node="$NODE" 2>&1) || rc=$?
    if [ "$rc" -ne 0 ]; then
        echo "ERROR: injectived tx wasm execute exited rc=$rc on $addr" >&2
        echo "  msg: $msg" >&2
        echo "$tx_output" >&2
        return 1
    fi
    txhash=$(echo "$tx_output" | grep -o 'txhash: [A-F0-9]*' | awk '{print $2}')
    if [ -z "$txhash" ]; then
        echo "ERROR: could not parse txhash from exec output:" >&2
        echo "$tx_output" >&2
        return 1
    fi
    wait_for_tx "$txhash" > /dev/null
    echo "$txhash"
}

# Broadcast `wasm set-contract-admin` and wait for indexing. Same rc-capture
# pattern as `exec_contract`.
#
# wasmd rejects `MsgUpdateAdmin` when the new admin equals the existing one
# (`new admin is the same as the old: invalid`). That trips on testnet
# self-rotations where the target == SIGNER_ADDRESS. Skip the broadcast when
# already at target so the script stays usable as a dry-run / validation harness.
set_admin() {
    local addr="$1"; local new_admin="$2"
    local current
    current=$(query_admin "$addr")
    if [ "$current" = "$new_admin" ]; then
        echo "  > set-contract-admin $addr already = $new_admin — skipping" >&2
        return 0
    fi
    echo "  > set-contract-admin $addr → $new_admin (was $current)" >&2
    local tx_output rc=0 txhash
    tx_output=$(printf '%s\n' "$PASSWORD" | injectived tx wasm set-contract-admin \
        "$addr" "$new_admin" \
        --from="$FROM" --chain-id="$CHAIN_ID" --yes \
        --fees="$FEES" --gas="$GAS" --node="$NODE" 2>&1) || rc=$?
    if [ "$rc" -ne 0 ]; then
        echo "ERROR: injectived tx wasm set-contract-admin exited rc=$rc on $addr → $new_admin" >&2
        echo "$tx_output" >&2
        return 1
    fi
    txhash=$(echo "$tx_output" | grep -o 'txhash: [A-F0-9]*' | awk '{print $2}')
    if [ -z "$txhash" ]; then
        echo "ERROR: could not parse txhash from set-contract-admin output:" >&2
        echo "$tx_output" >&2
        return 1
    fi
    wait_for_tx "$txhash" > /dev/null
}

# Query a contract's current wasm-admin. Echoes the address on stdout.
query_admin() {
    local addr="$1"
    injectived query wasm contract "$addr" --node="$NODE" 2>/dev/null \
        | grep -m1 'admin:' | awk '{print $2}'
}

# Read a single field from a contract's `{"config":{}}` response. Strips quotes.
query_config_field() {
    local addr="$1"; local field="$2"
    injectived query wasm contract-state smart "$addr" '{"config":{}}' \
        --node="$NODE" 2>/dev/null \
        | grep -m1 "$field:" | awk '{print $2}' | tr -d '"'
}
