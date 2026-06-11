#!/bin/bash
set -euo pipefail

# Full lazy mainnet/testnet deploy. One signer (EOA) does everything.
#
# Phase 1 — store 5 wasms:
#   choice_admin_timelock.wasm
#   choice_farm.wasm
#   choice_farm_factory.wasm
#   choice_zap_lp.wasm
#   choice_token_locker.wasm                (sourced from token-locker/artifacts/)
#
# Phase 2 — instantiate 4 contracts, all owned by the EOA:
#   admin_timelock, farm_factory, token_locker, zap_lp
#
# Phase 3 — rotate wasm admins:
#   factory   wasm-admin → TIMELOCK_ADDR        (preserves C-1 invariant)
#   timelock  wasm-admin → DEV_MULTISIG
#   token-locker wasm-admin → DEV_MULTISIG
#   zap-lp    wasm-admin → DEV_MULTISIG
#
# Phase 4 — rotate the two INSTANT config.owners to the multisig:
#   token-locker.config.admin → DEV_MULTISIG
#   zap-lp.config.owner       → DEV_MULTISIG
#
# DEFERRED (NOT done by this script — they require 48h propose+apply):
#   factory.config.owner   stays on EOA
#   timelock.config.owner  stays on EOA
# Run these later via `propose_new_owner` → wait timelock_seconds → `apply_owner_rotation`.
# Both `apply_owner_rotation`s are callable by the current EOA owner (or anyone, for timelock).
#
# Required env vars:
#   DEV_MULTISIG    final wasm-admin/config.owner for timelock, token-locker,
#                   zap-lp. On testnet, pass `$SIGNER_ADDRESS` to self-rotate.
#   TREASURY        receives factory launch fees + token-locker creation fees.
#                   On testnet, pass `$SIGNER_ADDRESS` if you don't have a
#                   separate treasury address.
#
# Optional env vars:
#   NETWORK              testnet | mainnet (default: testnet)
#   TIMELOCK_SECONDS     timelock propose/apply delay (default: 172800 = 48h).
#                        On testnet you can drop to 3600 (1h, contract floor).
#   FARM_FEE_INJ_BASE    CreateFarm launch fee, base units (default: 1e18 = 1 INJ)
#   LOCKER_FEE_AMOUNT    token-locker per-lock creation fee, base units
#                        (default: 1e17 = 0.1 INJ). Pass "0" to disable.
#   LOCKER_FEE_DENOM     denom for the locker creation fee (default: inj)
#   TOKEN_LOCKER_WASM    path to choice_token_locker.wasm
#                        (default: ../../token-locker/artifacts/choice_token_locker.wasm)
#
# Resume inputs (optional — pass these to skip already-completed phases):
#   TIMELOCK_CODE_ID, FARM_CODE_ID, FACTORY_CODE_ID,
#   ZAP_LP_CODE_ID, TOKEN_LOCKER_CODE_ID    all 5 to skip Phase 1 (stores)
#   TIMELOCK_ADDR, FACTORY_ADDR,
#   TOKEN_LOCKER_ADDR, ZAP_LP_ADDR          all 4 (plus the 5 code ids) to
#                                           skip Phase 2 (instantiates)
#
# On failure, the script prints a `resume hint` block to stderr with the
# env-var lines that would skip what's already done. Copy-paste into the
# next invocation to continue without paying for re-stores or re-instantiates.
#
# Usage examples:
#   # Testnet end-to-end with EOA standing in for multisig + treasury
#   NETWORK=testnet \
#       DEV_MULTISIG=inj1q2m26a7jdzjyfdn545vqsude3zwwtfrdap5jgz \
#       TREASURY=inj1q2m26a7jdzjyfdn545vqsude3zwwtfrdap5jgz \
#       TIMELOCK_SECONDS=3600 \
#       ./deploy/deploy_lazy_full.sh
#
#   # Mainnet
#   NETWORK=mainnet \
#       DEV_MULTISIG=inj1vcszz8j58m79exzdlpa8m9u5eyu9r37u7jhm7k \
#       TREASURY=inj1c2yleauy9say73tsx3dk5tvlgwwzdh96r76zv4 \
#       ./deploy/deploy_lazy_full.sh

source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

# ─── Inputs ────────────────────────────────────────────────────────────────
DEV_MULTISIG="${DEV_MULTISIG:-}"
TREASURY="${TREASURY:-}"
TIMELOCK_SECONDS="${TIMELOCK_SECONDS:-172800}"
FARM_FEE_INJ_BASE="${FARM_FEE_INJ_BASE:-1000000000000000000}"
LOCKER_FEE_AMOUNT="${LOCKER_FEE_AMOUNT:-100000000000000000}"
LOCKER_FEE_DENOM="${LOCKER_FEE_DENOM:-inj}"
ARTIFACTS_DIR="$(cd "$DEPLOY_DIR/.." && pwd)/artifacts"
TOKEN_LOCKER_WASM="${TOKEN_LOCKER_WASM:-$(cd "$DEPLOY_DIR/../.." 2>/dev/null && pwd)/token-locker/artifacts/choice_token_locker.wasm}"

# Optional resume inputs. Pass these to skip earlier phases:
#   * All 5 code-id env vars set → skip Phase 1 (stores).
#   * All 4 address env vars set → skip Phase 2 (instantiates).
#   * If you set the addresses but not the code ids, that's fine — rotation
#     phases don't need the code ids, they only need the addresses.
TIMELOCK_CODE_ID="${TIMELOCK_CODE_ID:-}"
FARM_CODE_ID="${FARM_CODE_ID:-}"
FACTORY_CODE_ID="${FACTORY_CODE_ID:-}"
ZAP_LP_CODE_ID="${ZAP_LP_CODE_ID:-}"
TOKEN_LOCKER_CODE_ID="${TOKEN_LOCKER_CODE_ID:-}"
TIMELOCK_ADDR="${TIMELOCK_ADDR:-}"
FACTORY_ADDR="${FACTORY_ADDR:-}"
TOKEN_LOCKER_ADDR="${TOKEN_LOCKER_ADDR:-}"
ZAP_LP_ADDR="${ZAP_LP_ADDR:-}"

# Decide which phases to run based on what was passed in. All-or-nothing per
# phase — a partial code-id or address set is almost certainly a copy-paste
# slip; refuse to proceed.
count_set() {
    local n=0; local v
    for v in "$@"; do [ -n "$v" ] && n=$((n+1)); done
    echo "$n"
}
phase1_set_count=$(count_set "$TIMELOCK_CODE_ID" "$FARM_CODE_ID" \
    "$FACTORY_CODE_ID" "$ZAP_LP_CODE_ID" "$TOKEN_LOCKER_CODE_ID")
phase2_set_count=$(count_set "$TIMELOCK_ADDR" "$FACTORY_ADDR" \
    "$TOKEN_LOCKER_ADDR" "$ZAP_LP_ADDR")

case "$phase1_set_count" in
    0) RUN_PHASE_1=1 ;;
    5) RUN_PHASE_1=0 ;;
    *)
        echo "ERROR: partial code-id set ($phase1_set_count of 5). Pass all five or none:" >&2
        echo "       TIMELOCK_CODE_ID FARM_CODE_ID FACTORY_CODE_ID ZAP_LP_CODE_ID TOKEN_LOCKER_CODE_ID" >&2
        exit 1
        ;;
esac
case "$phase2_set_count" in
    0) RUN_PHASE_2=1 ;;
    4) RUN_PHASE_2=0 ;;
    *)
        echo "ERROR: partial address set ($phase2_set_count of 4). Pass all four or none:" >&2
        echo "       TIMELOCK_ADDR FACTORY_ADDR TOKEN_LOCKER_ADDR ZAP_LP_ADDR" >&2
        exit 1
        ;;
esac
# Skipping Phase 2 implies Phase 1 was already done; require the code ids too
# so the final results JSON is complete.
if [ "$RUN_PHASE_2" = "0" ] && [ "$RUN_PHASE_1" = "1" ]; then
    echo "ERROR: addresses provided but code ids missing. Pass all 5 code ids" >&2
    echo "       alongside the 4 addresses so the results JSON is complete." >&2
    exit 1
fi

# ─── Validation ────────────────────────────────────────────────────────────
[ -n "$DEV_MULTISIG" ] || { echo "ERROR: DEV_MULTISIG is required" >&2; exit 1; }
[ -n "$TREASURY" ]     || { echo "ERROR: TREASURY is required"     >&2; exit 1; }
if ! [[ "$TIMELOCK_SECONDS" =~ ^[0-9]+$ ]] || [ "$TIMELOCK_SECONDS" -lt 3600 ]; then
    echo "ERROR: TIMELOCK_SECONDS=$TIMELOCK_SECONDS must be an integer ≥ 3600" >&2
    exit 1
fi
if ! [[ "$FARM_FEE_INJ_BASE" =~ ^[0-9]+$ ]]; then
    echo "ERROR: FARM_FEE_INJ_BASE='$FARM_FEE_INJ_BASE' must be a non-negative integer" >&2
    exit 1
fi
if ! [[ "$LOCKER_FEE_AMOUNT" =~ ^[0-9]+$ ]]; then
    echo "ERROR: LOCKER_FEE_AMOUNT='$LOCKER_FEE_AMOUNT' must be a non-negative integer" >&2
    exit 1
fi

REQUIRED_WASMS=(
    "$ARTIFACTS_DIR/choice_admin_timelock.wasm"
    "$ARTIFACTS_DIR/choice_farm.wasm"
    "$ARTIFACTS_DIR/choice_farm_factory.wasm"
    "$ARTIFACTS_DIR/choice_zap_lp.wasm"
    "$TOKEN_LOCKER_WASM"
)
for wasm in "${REQUIRED_WASMS[@]}"; do
    if [ ! -f "$wasm" ]; then
        echo "ERROR: missing wasm: $wasm" >&2
        echo "  Run ./build_release.sh in BOTH choice_exchange/ AND token-locker/." >&2
        exit 1
    fi
done

# ─── Helpers (local to this script — not promoted to lib.sh yet) ───────────
# Broadcast a plain `wasm execute` and wait for indexing. Aborts on chain-level
# tx failure. Echoes the txhash on success.
#
# Note: `injectived` may exit non-zero before producing a txhash (e.g. account
# sequence mismatch surfaced locally). With `set -e`, a bare `tx_output=$(...)`
# whose subshell fails would kill the script silently before we can read
# `tx_output`. Capture rc explicitly so we always surface the underlying
# stderr.
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
# pattern as `exec_contract` — see that comment for why.
#
# wasmd rejects `MsgUpdateAdmin` when the new admin equals the existing one
# (`new admin is the same as the old: invalid` at wasmd/x/wasm/types/tx.go).
# That trips on testnet self-rotations where DEV_MULTISIG == SIGNER_ADDRESS.
# Skip the broadcast when already at target so the script stays usable as a
# dry-run / validation harness.
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

# ─── Banner ────────────────────────────────────────────────────────────────
banner "🚀 FULL LAZY DEPLOY"
echo "  Signer (EOA):                            $SIGNER_ADDRESS ($FROM)"
if [ "$STORE_FROM" != "$FROM" ]; then
    echo "  Phase 1 store signer (separate key):     $STORE_FROM"
fi
echo "  Dev multisig (rotation target):          $DEV_MULTISIG"
echo "  Treasury (fee_collector):                $TREASURY"
echo "  Timelock seconds:                        $TIMELOCK_SECONDS"
echo "  Farm launch fee (inj base):              $FARM_FEE_INJ_BASE"
echo "  Token-locker creation fee:               $LOCKER_FEE_AMOUNT $LOCKER_FEE_DENOM"
echo "  Token-locker wasm:                       $TOKEN_LOCKER_WASM"
echo " "
if [ "$DEV_MULTISIG" = "$SIGNER_ADDRESS" ]; then
    echo "  ℹ️  DEV_MULTISIG == SIGNER_ADDRESS — rotation steps will self-rotate."
    echo "      Fine for testnet validation; surface this in mainnet pre-flight."
    echo " "
fi
if [ "$RUN_PHASE_1" = "0" ]; then
    echo "  ↪ Phase 1 (stores) skipped — using passed-in code ids."
fi
if [ "$RUN_PHASE_2" = "0" ]; then
    echo "  ↪ Phase 2 (instantiates) skipped — using passed-in addresses."
fi
if [ "$RUN_PHASE_1" = "0" ] || [ "$RUN_PHASE_2" = "0" ]; then
    echo " "
fi

# ─── Phase 1: stores ───────────────────────────────────────────────────────
if [ "$RUN_PHASE_1" = "1" ]; then
    echo "═══════════════════════════════════════════════════"
    echo "  Phase 1/4: store wasms"
    echo "═══════════════════════════════════════════════════"
    TIMELOCK_CODE_ID=$(store_contract "$ARTIFACTS_DIR/choice_admin_timelock.wasm")
    echo "  TIMELOCK_CODE_ID=$TIMELOCK_CODE_ID"
    FARM_CODE_ID=$(store_contract "$ARTIFACTS_DIR/choice_farm.wasm")
    echo "  FARM_CODE_ID=$FARM_CODE_ID"
    FACTORY_CODE_ID=$(store_contract "$ARTIFACTS_DIR/choice_farm_factory.wasm")
    echo "  FACTORY_CODE_ID=$FACTORY_CODE_ID"
    ZAP_LP_CODE_ID=$(store_contract "$ARTIFACTS_DIR/choice_zap_lp.wasm")
    echo "  ZAP_LP_CODE_ID=$ZAP_LP_CODE_ID"
    TOKEN_LOCKER_CODE_ID=$(store_contract "$TOKEN_LOCKER_WASM")
    echo "  TOKEN_LOCKER_CODE_ID=$TOKEN_LOCKER_CODE_ID"
    echo " "

    # If anything below fails, surface the captured ids so the operator can
    # resume without re-storing the wasms (5 × store-fee saved).
    trap '
        rc=$?
        if [ "$rc" -ne 0 ]; then
            echo " " >&2
            echo "─── resume hint (pass these in to skip Phase 1) ───" >&2
            echo "TIMELOCK_CODE_ID=$TIMELOCK_CODE_ID" >&2
            echo "FARM_CODE_ID=$FARM_CODE_ID" >&2
            echo "FACTORY_CODE_ID=$FACTORY_CODE_ID" >&2
            echo "ZAP_LP_CODE_ID=$ZAP_LP_CODE_ID" >&2
            echo "TOKEN_LOCKER_CODE_ID=$TOKEN_LOCKER_CODE_ID" >&2
            echo " " >&2
        fi
    ' EXIT
fi

# ─── Phase 2: instantiates (EOA owns everything) ───────────────────────────
if [ "$RUN_PHASE_2" = "1" ]; then
    echo "═══════════════════════════════════════════════════"
    echo "  Phase 2/4: instantiate (EOA owns all)"
    echo "═══════════════════════════════════════════════════"

    # Admin timelock — owner=EOA, --admin=EOA.
    TIMELOCK_INIT=$(printf '{"owner":"%s","timelock_seconds":%d}' \
        "$SIGNER_ADDRESS" "$TIMELOCK_SECONDS")
    TIMELOCK_ADDR=$(instantiate_contract "$TIMELOCK_CODE_ID" "$TIMELOCK_INIT" \
        "Choice Admin Timelock (lazy)" "$SIGNER_ADDRESS")
    echo "  TIMELOCK_ADDR=$TIMELOCK_ADDR"

    # Farm factory — OWNER=EOA, FARM_OWNER=EOA, --admin=EOA. C-1 holds because
    # wasm-admin == farm_owner == EOA. FEE_COLLECTOR points at treasury directly.
    FACTORY_INIT=$(cat <<EOF
{
  "owner": "$SIGNER_ADDRESS",
  "fee_collector": "$TREASURY",
  "instantiate_fee_inj": "$FARM_FEE_INJ_BASE",
  "farm_code_id": $FARM_CODE_ID,
  "farm_owner": "$SIGNER_ADDRESS"
}
EOF
)
    FACTORY_ADDR=$(instantiate_contract "$FACTORY_CODE_ID" "$FACTORY_INIT" \
        "Choice Farm Factory (lazy)" "$SIGNER_ADDRESS")
    echo "  FACTORY_ADDR=$FACTORY_ADDR"

    # Token locker — Config.admin=EOA, fee_collector=treasury, --admin=EOA.
    if [ "$LOCKER_FEE_AMOUNT" = "0" ]; then
        LOCKER_INIT=$(printf '{"admin":"%s","fee_collector":"%s","creation_fee":null}' \
            "$SIGNER_ADDRESS" "$TREASURY")
    else
        LOCKER_INIT=$(printf '{"admin":"%s","fee_collector":"%s","creation_fee":{"denom":"%s","amount":"%s"}}' \
            "$SIGNER_ADDRESS" "$TREASURY" "$LOCKER_FEE_DENOM" "$LOCKER_FEE_AMOUNT")
    fi
    TOKEN_LOCKER_ADDR=$(instantiate_contract "$TOKEN_LOCKER_CODE_ID" "$LOCKER_INIT" \
        "Choice Token Locker (lazy)" "$SIGNER_ADDRESS")
    echo "  TOKEN_LOCKER_ADDR=$TOKEN_LOCKER_ADDR"

    # Zap LP — owner=EOA, no default_recipient (user-zap only), tip_bps=0,
    # min_zap_amount=0, --admin=EOA.
    ZAP_LP_INIT=$(printf '{"owner":"%s","tip_bps":0,"min_zap_amount":"0"}' \
        "$SIGNER_ADDRESS")
    ZAP_LP_ADDR=$(instantiate_contract "$ZAP_LP_CODE_ID" "$ZAP_LP_INIT" \
        "Choice Zap LP (lazy)" "$SIGNER_ADDRESS")
    echo "  ZAP_LP_ADDR=$ZAP_LP_ADDR"
    echo " "
fi  # end RUN_PHASE_2

# Extend the resume hint to include addresses for any failure past this point.
# Overrides the Phase-1-only trap installed earlier.
trap '
    rc=$?
    if [ "$rc" -ne 0 ]; then
        echo " " >&2
        echo "─── resume hint (pass these in to skip Phases 1+2) ───" >&2
        [ -n "$TIMELOCK_CODE_ID" ]     && echo "TIMELOCK_CODE_ID=$TIMELOCK_CODE_ID" >&2
        [ -n "$FARM_CODE_ID" ]         && echo "FARM_CODE_ID=$FARM_CODE_ID" >&2
        [ -n "$FACTORY_CODE_ID" ]      && echo "FACTORY_CODE_ID=$FACTORY_CODE_ID" >&2
        [ -n "$ZAP_LP_CODE_ID" ]       && echo "ZAP_LP_CODE_ID=$ZAP_LP_CODE_ID" >&2
        [ -n "$TOKEN_LOCKER_CODE_ID" ] && echo "TOKEN_LOCKER_CODE_ID=$TOKEN_LOCKER_CODE_ID" >&2
        [ -n "$TIMELOCK_ADDR" ]        && echo "TIMELOCK_ADDR=$TIMELOCK_ADDR" >&2
        [ -n "$FACTORY_ADDR" ]         && echo "FACTORY_ADDR=$FACTORY_ADDR" >&2
        [ -n "$TOKEN_LOCKER_ADDR" ]    && echo "TOKEN_LOCKER_ADDR=$TOKEN_LOCKER_ADDR" >&2
        [ -n "$ZAP_LP_ADDR" ]          && echo "ZAP_LP_ADDR=$ZAP_LP_ADDR" >&2
        echo " " >&2
    fi
' EXIT

# ─── Phase 3: wasm-admin rotations ─────────────────────────────────────────
echo "═══════════════════════════════════════════════════"
echo "  Phase 3/4: rotate wasm admins"
echo "═══════════════════════════════════════════════════"

# C-1 invariant: factory wasm-admin must equal config.farm_owner.
# Initial state: both EOA. To minimise the broken window, flip farm_owner
# first (UpdateConfig is instant), then immediately follow with the
# set-contract-admin. Both go to TIMELOCK_ADDR.
echo "  [factory] update_config { farm_owner: TIMELOCK_ADDR }"
exec_contract "$FACTORY_ADDR" \
    "{\"update_config\":{\"farm_owner\":\"$TIMELOCK_ADDR\"}}" \
    "factory.update_config(farm_owner)" > /dev/null

set_admin "$FACTORY_ADDR" "$TIMELOCK_ADDR"

# The remaining three contracts' wasm admins go straight to the multisig.
set_admin "$TIMELOCK_ADDR"     "$DEV_MULTISIG"
set_admin "$TOKEN_LOCKER_ADDR" "$DEV_MULTISIG"
set_admin "$ZAP_LP_ADDR"       "$DEV_MULTISIG"
echo " "

# ─── Phase 4: instant config.owner rotations ───────────────────────────────
echo "═══════════════════════════════════════════════════"
echo "  Phase 4/4: rotate instant config.owners"
echo "═══════════════════════════════════════════════════"

echo "  [token-locker] update_config { admin: DEV_MULTISIG }"
exec_contract "$TOKEN_LOCKER_ADDR" \
    "{\"update_config\":{\"admin\":\"$DEV_MULTISIG\"}}" \
    "token-locker.update_config(admin)" > /dev/null

echo "  [zap-lp] update_config { owner: DEV_MULTISIG }"
exec_contract "$ZAP_LP_ADDR" \
    "{\"update_config\":{\"owner\":\"$DEV_MULTISIG\"}}" \
    "zap-lp.update_config(owner)" > /dev/null
echo " "

# ─── Verify ────────────────────────────────────────────────────────────────
echo "═══════════════════════════════════════════════════"
echo "  Verify on-chain state"
echo "═══════════════════════════════════════════════════"
fail=0
verify_admin() {
    local addr="$1"; local expected="$2"; local label="$3"
    local actual
    actual=$(query_admin "$addr")
    if [ "$actual" = "$expected" ]; then
        echo "  ✅ $label wasm-admin = $expected"
    else
        echo "  ❌ $label wasm-admin = '$actual' (expected $expected)" >&2
        fail=1
    fi
}
verify_admin "$TIMELOCK_ADDR"     "$DEV_MULTISIG"  "timelock      "
verify_admin "$FACTORY_ADDR"      "$TIMELOCK_ADDR" "factory       "
verify_admin "$TOKEN_LOCKER_ADDR" "$DEV_MULTISIG"  "token-locker  "
verify_admin "$ZAP_LP_ADDR"       "$DEV_MULTISIG"  "zap-lp        "

# Factory config: farm_owner must == TIMELOCK_ADDR (C-1 enforcement).
FACTORY_FARM_OWNER=$(query_config_field "$FACTORY_ADDR" "farm_owner")
if [ "$FACTORY_FARM_OWNER" = "$TIMELOCK_ADDR" ]; then
    echo "  ✅ factory.farm_owner    = $TIMELOCK_ADDR (C-1 aligned)"
else
    echo "  ❌ factory.farm_owner    = '$FACTORY_FARM_OWNER' (expected $TIMELOCK_ADDR) — C-1 BROKEN" >&2
    fail=1
fi

# Token locker H-2: wasm-admin must == Config.admin (operator-enforced now that
# the contract's self-query was removed — see token-locker/contracts/locker/
# src/contract.rs:53 for why).
LOCKER_CONFIG_ADMIN=$(query_config_field "$TOKEN_LOCKER_ADDR" "admin")
LOCKER_WASM_ADMIN=$(query_admin "$TOKEN_LOCKER_ADDR")
if [ "$LOCKER_WASM_ADMIN" = "$LOCKER_CONFIG_ADMIN" ]; then
    echo "  ✅ token-locker H-2     wasm-admin == config.admin == $LOCKER_WASM_ADMIN"
else
    echo "  ❌ token-locker H-2 BROKEN: wasm-admin=$LOCKER_WASM_ADMIN config.admin=$LOCKER_CONFIG_ADMIN" >&2
    fail=1
fi

if [ "$fail" -ne 0 ]; then
    echo " " >&2
    echo "ERROR: verification failed. The deploy completed but at least one" >&2
    echo "       rotation didn't land — investigate before announcing addresses." >&2
    exit 1
fi
echo " "

# ─── Results ───────────────────────────────────────────────────────────────
RESULTS_DIR="$DEPLOY_DIR/results"
mkdir -p "$RESULTS_DIR"
RESULTS_FILE="$RESULTS_DIR/${NETWORK}_$(date +%Y%m%d_%H%M%S).json"
cat > "$RESULTS_FILE" <<EOF
{
  "network": "$NETWORK",
  "chain_id": "$CHAIN_ID",
  "deployed_at_utc": "$(date -u +%Y-%m-%dT%H:%M:%SZ)",
  "signer": "$SIGNER_ADDRESS",
  "dev_multisig": "$DEV_MULTISIG",
  "treasury": "$TREASURY",
  "timelock_seconds": $TIMELOCK_SECONDS,
  "farm_fee_inj_base": "$FARM_FEE_INJ_BASE",
  "token_locker_creation_fee": {
    "amount": "$LOCKER_FEE_AMOUNT",
    "denom": "$LOCKER_FEE_DENOM"
  },
  "code_ids": {
    "admin_timelock":  $TIMELOCK_CODE_ID,
    "farm":            $FARM_CODE_ID,
    "farm_factory":    $FACTORY_CODE_ID,
    "zap_lp":          $ZAP_LP_CODE_ID,
    "token_locker":    $TOKEN_LOCKER_CODE_ID
  },
  "addresses": {
    "admin_timelock":  "$TIMELOCK_ADDR",
    "farm_factory":    "$FACTORY_ADDR",
    "zap_lp":          "$ZAP_LP_ADDR",
    "token_locker":    "$TOKEN_LOCKER_ADDR"
  },
  "deferred_rotations": [
    "factory.config.owner  $SIGNER_ADDRESS → $DEV_MULTISIG  (propose_new_owner + 48h + apply_owner_rotation)",
    "timelock.config.owner $SIGNER_ADDRESS → $DEV_MULTISIG  (propose_new_owner + ${TIMELOCK_SECONDS}s + apply_owner_rotation)"
  ]
}
EOF

banner "✅ LAZY DEPLOY COMPLETE"
echo "Results: $RESULTS_FILE"
echo " "
cat "$RESULTS_FILE"
echo " "
echo "─── deployed_contracts.md paste ───────────────────"
echo "| admin_timelock  | $TIMELOCK_CODE_ID | $TIMELOCK_ADDR |"
echo "| farm            | $FARM_CODE_ID | n/a (spawned by factory) |"
echo "| farm_factory    | $FACTORY_CODE_ID | $FACTORY_ADDR |"
echo "| zap_lp          | $ZAP_LP_CODE_ID | $ZAP_LP_ADDR |"
echo "| token_locker    | $TOKEN_LOCKER_CODE_ID | $TOKEN_LOCKER_ADDR |"
echo " "
echo "─── deferred owner-rotation kickoff (whenever) ────"
echo "injectived tx wasm execute $FACTORY_ADDR \\"
echo "    '{\"propose_new_owner\":{\"new_owner\":\"$DEV_MULTISIG\"}}' \\"
echo "    --from=$FROM --chain-id=$CHAIN_ID --fees=$FEES --gas=$GAS --node=$NODE -y"
echo "injectived tx wasm execute $TIMELOCK_ADDR \\"
echo "    '{\"propose_new_owner\":{\"new_owner\":\"$DEV_MULTISIG\"}}' \\"
echo "    --from=$FROM --chain-id=$CHAIN_ID --fees=$FEES --gas=$GAS --node=$NODE -y"
echo "# … wait ${TIMELOCK_SECONDS}s, then:"
echo "injectived tx wasm execute $FACTORY_ADDR  '{\"apply_owner_rotation\":{}}' --from=$FROM --chain-id=$CHAIN_ID --fees=$FEES --gas=$GAS --node=$NODE -y"
echo "injectived tx wasm execute $TIMELOCK_ADDR '{\"apply_owner_rotation\":{}}' --from=$FROM --chain-id=$CHAIN_ID --fees=$FEES --gas=$GAS --node=$NODE -y"
echo " "
