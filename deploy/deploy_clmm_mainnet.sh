#!/bin/bash
set -euo pipefail

# ─────────────────────────────────────────────────────────────────────────────
# CLMM mainnet deploy — phased + resumable, modeled on deploy_lazy_full.sh.
#
# Stores + instantiates the CLMM stack (pool / factory / manager), rotates the
# factory + manager wasm-admins to the Admin Timelock, and (optionally) proposes
# the factory owner → Dev Multisig. Pools are created later via the frontend
# (CreatePool is permissionless), so this script does NOT create pools.
#
# Sourced config (deploy/network/${NETWORK}.env via lib.sh):
#   NODE CHAIN_ID GAS FEES FROM PASSWORD SIGNER_ADDRESS  (+ STORE_FROM/STORE_PASSWORD)
#
# Required env (script-specific):
#   ADMIN_TIMELOCK_ADDR   wasm-admin target for factory + manager (48h migrate delay).
#   DEV_MULTISIG          factory owner rotation target (only consumed if ROTATE_OWNER=1).
#
# Optional env:
#   ROTATE_OWNER=1        propose factory owner → DEV_MULTISIG now (default: 0, deferred;
#                         multisig signers may be offline — accept happens later).
#   POOL_CODE_ID / FACTORY_CODE_ID / MANAGER_CODE_ID   all 3 → skip Phase 1 (stores).
#   FACTORY_ADDR / MANAGER_ADDR                        both → skip Phase 2 (instantiates).
#
# Example (mainnet, deferred owner rotation = launch default):
#   NETWORK=mainnet \
#   PASSWORD=… \
#   ADMIN_TIMELOCK_ADDR=inj14tm9kjh396g483aj76xyykem2mdk22q8x769v9 \
#   DEV_MULTISIG=inj1vcszz8j58m79exzdlpa8m9u5eyu9r37u7jhm7k \
#       ./deploy_clmm_mainnet.sh
# ─────────────────────────────────────────────────────────────────────────────

source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

ARTIFACTS_DIR="$(cd "$DEPLOY_DIR/../artifacts" && pwd)"

ADMIN_TIMELOCK_ADDR="${ADMIN_TIMELOCK_ADDR:-}"
DEV_MULTISIG="${DEV_MULTISIG:-}"
ROTATE_OWNER="${ROTATE_OWNER:-0}"

POOL_CODE_ID="${POOL_CODE_ID:-}"
FACTORY_CODE_ID="${FACTORY_CODE_ID:-}"
MANAGER_CODE_ID="${MANAGER_CODE_ID:-}"
FACTORY_ADDR="${FACTORY_ADDR:-}"
MANAGER_ADDR="${MANAGER_ADDR:-}"

[ -n "$ADMIN_TIMELOCK_ADDR" ] || { echo "ERROR: ADMIN_TIMELOCK_ADDR is required" >&2; exit 1; }
if [ "$ROTATE_OWNER" = "1" ] && [ -z "$DEV_MULTISIG" ]; then
    echo "ERROR: ROTATE_OWNER=1 requires DEV_MULTISIG" >&2; exit 1
fi

# The CLMM wasms are large (~640KB pool) — storing one needs ~4.2M gas, well
# above the lib.sh env defaults (3.5-3.7M, tuned for the lighter legacy
# contracts). Override GAS/FEES here so the stores don't hit `out of gas`
# (code=11). Instantiate/exec/admin txs are cheaper but run fine at this limit
# (you pay the fixed FEES, not gasUsed). Override via CLMM_GAS / CLMM_FEES.
GAS="${CLMM_GAS:-8000000}"
FEES="${CLMM_FEES:-4000000000000000inj}"

# ─── Phase gating (same convention as lazy_full) ─────────────────────────────
count_set() { local n=0; for v in "$@"; do [ -n "$v" ] && n=$((n+1)); done; echo "$n"; }
p1_set=$(count_set "$POOL_CODE_ID" "$FACTORY_CODE_ID" "$MANAGER_CODE_ID")
p2_set=$(count_set "$FACTORY_ADDR" "$MANAGER_ADDR")
case "$p1_set" in
    0) RUN_PHASE_1=1 ;;
    3) RUN_PHASE_1=0 ;;
    *) echo "ERROR: set ALL or NONE of POOL_CODE_ID FACTORY_CODE_ID MANAGER_CODE_ID" >&2; exit 1 ;;
esac
case "$p2_set" in
    0) RUN_PHASE_2=1 ;;
    2) RUN_PHASE_2=0 ;;
    *) echo "ERROR: set BOTH or NEITHER of FACTORY_ADDR MANAGER_ADDR" >&2; exit 1 ;;
esac
# Skipping Phase 2 implies Phase 1 was done — require the code ids too.
if [ "$RUN_PHASE_2" = "0" ] && [ "$RUN_PHASE_1" = "1" ]; then
    echo "ERROR: FACTORY_ADDR/MANAGER_ADDR given but code ids missing — also pass" >&2
    echo "       POOL_CODE_ID FACTORY_CODE_ID MANAGER_CODE_ID" >&2
    exit 1
fi

banner "🌊 CLMM MAINNET DEPLOY"
echo "  Signer (EOA):                $SIGNER_ADDRESS ($FROM)"
if [ "$STORE_FROM" != "$FROM" ]; then
    echo "  Phase 1 store signer:        $STORE_FROM"
fi
echo "  wasm-admin target:           $ADMIN_TIMELOCK_ADDR (Admin Timelock)"
echo "  Owner rotation target:       ${DEV_MULTISIG:-(unset)}"
echo "  Rotate owner now:            $([ "$ROTATE_OWNER" = 1 ] && echo "yes" || echo "NO — deferred")"
echo " "

# ─── Phase 1 — store ─────────────────────────────────────────────────────────
if [ "$RUN_PHASE_1" = "1" ]; then
    banner "PHASE 1 — store CLMM wasms"
    echo "Storing pool…";    POOL_CODE_ID=$(store_contract "$ARTIFACTS_DIR/choice_clmm_pool.wasm")
    echo "  pool code id:    $POOL_CODE_ID"
    echo "Storing factory…"; FACTORY_CODE_ID=$(store_contract "$ARTIFACTS_DIR/choice_clmm_factory.wasm")
    echo "  factory code id: $FACTORY_CODE_ID"
    echo "Storing manager…"; MANAGER_CODE_ID=$(store_contract "$ARTIFACTS_DIR/choice_clmm_manager.wasm")
    echo "  manager code id: $MANAGER_CODE_ID"
else
    echo "Phase 1 skipped — pool=$POOL_CODE_ID factory=$FACTORY_CODE_ID manager=$MANAGER_CODE_ID"
fi

# ─── Phase 2 — instantiate (admin = EOA for the bring-up window) ─────────────
if [ "$RUN_PHASE_2" = "1" ]; then
    banner "PHASE 2 — instantiate factory + manager"
    # Factory: owner = instantiator (EOA); default fee tiers 100/500/3000/10000
    # are auto-created. wasm-admin starts as EOA, rotated to the timelock in P3.
    FACTORY_ADDR=$(instantiate_contract "$FACTORY_CODE_ID" \
        "{\"pool_code_id\": $POOL_CODE_ID}" \
        "Choice CLMM Factory" "$SIGNER_ADDRESS")
    echo "  factory: $FACTORY_ADDR"
    MANAGER_ADDR=$(instantiate_contract "$MANAGER_CODE_ID" \
        "{\"name\":\"Choice CL Positions\",\"symbol\":\"CHOICE-CLP\",\"factory_addr\":\"$FACTORY_ADDR\"}" \
        "Choice CLMM Manager" "$SIGNER_ADDRESS")
    echo "  manager: $MANAGER_ADDR"
else
    echo "Phase 2 skipped — factory=$FACTORY_ADDR manager=$MANAGER_ADDR"
fi

# ─── Phase 3 — rotate wasm-admins → Admin Timelock ──────────────────────────
banner "PHASE 3 — rotate wasm-admins → Admin Timelock"
set_admin "$FACTORY_ADDR" "$ADMIN_TIMELOCK_ADDR"
set_admin "$MANAGER_ADDR" "$ADMIN_TIMELOCK_ADDR"

# ─── Phase 4 — owner rotation (DEFERRED by default) ─────────────────────────
banner "PHASE 4 — factory owner → Dev Multisig"
if [ "$ROTATE_OWNER" = "1" ]; then
    PROPOSE_TX=$(exec_contract "$FACTORY_ADDR" \
        "{\"propose_owner\":{\"new_owner\":\"$DEV_MULTISIG\"}}" "propose_owner")
    echo "  proposed owner → $DEV_MULTISIG (tx $PROPOSE_TX)"
    echo "  Owner stays EOA until the multisig accepts. Multisig must run:"
    echo "      injectived tx wasm execute $FACTORY_ADDR '{\"accept_owner\":{}}' \\"
    echo "          --from <DEV_MULTISIG> --chain-id=$CHAIN_ID --fees=$FEES --gas=$GAS --node=$NODE"
else
    PROPOSE_TX=""
    echo "  DEFERRED — owner stays the EOA ($SIGNER_ADDRESS)."
    echo "  When multisig signers are online, run the propose+accept pair:"
    echo "      # 1. EOA proposes:"
    echo "      injectived tx wasm execute $FACTORY_ADDR \\"
    echo "          '{\"propose_owner\":{\"new_owner\":\"${DEV_MULTISIG:-inj1<dev_multisig>}\"}}' \\"
    echo "          --from=$FROM --chain-id=$CHAIN_ID --fees=$FEES --gas=$GAS --node=$NODE"
    echo "      # 2. Multisig accepts:"
    echo "      injectived tx wasm execute $FACTORY_ADDR '{\"accept_owner\":{}}' \\"
    echo "          --from <DEV_MULTISIG> --chain-id=$CHAIN_ID --fees=$FEES --gas=$GAS --node=$NODE"
fi

# ─── Verify ──────────────────────────────────────────────────────────────────
banner "VERIFY"
echo "  factory wasm-admin: $(query_admin "$FACTORY_ADDR")"
echo "  manager wasm-admin: $(query_admin "$MANAGER_ADDR")"
FACTORY_OWNER=$(injectived query wasm contract-state smart "$FACTORY_ADDR" '{"get_config":{}}' \
    --node="$NODE" 2>/dev/null | grep -m1 'owner:' | awk '{print $2}' | tr -d '"' || true)
echo "  factory owner:      ${FACTORY_OWNER:-?} (expect EOA until rotation accepted)"
# cw721 creator can default to the instantiator (EOA) — surface it so it can be
# handed to the multisig if it is non-trivial.
MGR_CREATOR=$(injectived query wasm contract-state smart "$MANAGER_ADDR" '{"ownership":{}}' \
    --node="$NODE" 2>/dev/null | grep -m1 'owner:' | awk '{print $2}' | tr -d '"' || true)
echo "  manager cw721 owner/creator: ${MGR_CREATOR:-<none>} (hand to multisig if = EOA)"

# ─── Results ─────────────────────────────────────────────────────────────────
RESULTS_DIR="$DEPLOY_DIR/results"
mkdir -p "$RESULTS_DIR"
RESULTS_FILE="$RESULTS_DIR/${NETWORK}_clmm_$(date +%Y%m%d_%H%M%S).json"
cat > "$RESULTS_FILE" <<EOF
{
  "network": "$NETWORK",
  "chain_id": "$CHAIN_ID",
  "deployed_at_utc": "$(date -u +%Y-%m-%dT%H:%M:%SZ)",
  "signer": "$SIGNER_ADDRESS",
  "admin_timelock": "$ADMIN_TIMELOCK_ADDR",
  "dev_multisig": "${DEV_MULTISIG:-}",
  "owner_rotated": $([ "$ROTATE_OWNER" = 1 ] && echo true || echo false),
  "code_ids": {
    "pool":    $POOL_CODE_ID,
    "factory": $FACTORY_CODE_ID,
    "manager": $MANAGER_CODE_ID
  },
  "addresses": {
    "factory": "$FACTORY_ADDR",
    "manager": "$MANAGER_ADDR"
  },
  "propose_owner_tx": "${PROPOSE_TX}",
  "deferred": [
    "factory.config.owner $SIGNER_ADDRESS → ${DEV_MULTISIG:-<dev_multisig>} (propose_owner + multisig accept_owner)"
  ]
}
EOF

banner "✅ CLMM MAINNET DEPLOY COMPLETE"
echo "Results: $RESULTS_FILE"
echo " "
echo "Next:"
echo "  • FE env: VITE_CLMM_FACTORY_ADDRESS=$FACTORY_ADDR"
echo "            VITE_CLMM_MANAGER_ADDRESS=$MANAGER_ADDR"
echo "  • Pools created via the FE launch fee-free (protocol carve defaults OFF)."
echo "  • Flash stays deny-all until: aggregator AuthorizeFlashSigner(<bot>) AND"
echo "    factory owner AuthorizeFlashBorrower(<aggregator>)."
echo "  • Rotate factory owner → multisig when signers are online (see Phase 4)."
cat "$RESULTS_FILE"
