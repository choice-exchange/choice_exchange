#!/bin/bash
set -euo pipefail

# Instantiate a choice_claim_drops hub.
#
# Required env var (or positional arg 1):
#   CLAIM_DROPS_CODE_ID   code id from ./deploy/upload_claim_drops_code.sh
#
# Optional env vars (with defaults):
#   NETWORK         testnet | mainnet (default: testnet)
#   OWNER           per-instance owner: may create streaming campaigns, run
#                   UpdateConfig / Rescue, and freeze a stream. One-shot drops
#                   are permissionless regardless. Default: $SIGNER_ADDRESS.
#   FEE_BPS         platform fee in bps charged ON TOP of each funding delta,
#                   paid to FEE_COLLECTOR. Contract caps it at 1000 (10%).
#                   Default: 0 — the trippytools tool charges its own SHROOM
#                   fee off-chain, so the on-chain fee stays off.
#   FEE_COLLECTOR   recipient of the platform fee. Default: $OWNER.
#   LABEL           wasm contract label (default: "Choice Claim Drops v1.1.0")
#   ADMIN           wasm admin for this instance. Default: $SIGNER_ADDRESS.
#
# Usage:
#   NETWORK=testnet CLAIM_DROPS_CODE_ID=1234 ./deploy/instantiate_claim_drops.sh
#   ./deploy/instantiate_claim_drops.sh 1234           # positional, testnet

source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

CLAIM_DROPS_CODE_ID="${CLAIM_DROPS_CODE_ID:-${1:-}}"
OWNER="${OWNER:-$SIGNER_ADDRESS}"
FEE_BPS="${FEE_BPS:-0}"
FEE_COLLECTOR="${FEE_COLLECTOR:-$OWNER}"
LABEL="${LABEL:-Choice Claim Drops v1.1.0}"
ADMIN="${ADMIN:-$SIGNER_ADDRESS}"

if [ -z "$CLAIM_DROPS_CODE_ID" ]; then
    echo "ERROR: CLAIM_DROPS_CODE_ID is required."
    echo "       Pass via env var or positional arg 1."
    echo "       Run ./deploy/upload_claim_drops_code.sh first to get it."
    exit 1
fi
if ! [[ "$CLAIM_DROPS_CODE_ID" =~ ^[0-9]+$ ]]; then
    echo "ERROR: CLAIM_DROPS_CODE_ID '$CLAIM_DROPS_CODE_ID' is not an integer."; exit 1
fi
if ! [[ "$FEE_BPS" =~ ^[0-9]+$ ]]; then
    echo "ERROR: FEE_BPS '$FEE_BPS' is not an integer."; exit 1
fi
if [ "$FEE_BPS" -gt 1000 ]; then
    echo "ERROR: FEE_BPS=$FEE_BPS exceeds the contract's 1000 bps (10%) cap."
    exit 1
fi

banner "🎁 INSTANTIATE CHOICE CLAIM DROPS"
echo "  Code id:       $CLAIM_DROPS_CODE_ID"
echo "  Owner:         $OWNER"
echo "  Fee bps:       $FEE_BPS"
echo "  Fee collector: $FEE_COLLECTOR"
echo "  Wasm admin:    $ADMIN"
echo " "

# fee_bps is u16 → JSON number, not string.
INIT_MSG=$(cat <<EOF
{
  "owner": "$OWNER",
  "fee_bps": $FEE_BPS,
  "fee_collector": "$FEE_COLLECTOR"
}
EOF
)

echo "-------------------------------------------------"
echo "  InstantiateMsg:"
echo "-------------------------------------------------"
echo "$INIT_MSG"
echo " "

echo "-------------------------------------------------"
echo "  Broadcasting instantiate..."
echo "-------------------------------------------------"
CLAIM_DROPS_ADDR=$(instantiate_contract "$CLAIM_DROPS_CODE_ID" "$INIT_MSG" "$LABEL" "$ADMIN")

echo " "
echo "-------------------------------------------------"
echo "  Config query"
echo "-------------------------------------------------"
injectived query wasm contract-state smart "$CLAIM_DROPS_ADDR" '{"config":{}}' \
    --node="$NODE" --output json | python3 -m json.tool
echo " "

echo "================================================="
echo "  ✅ CLAIM DROPS INSTANTIATED"
echo "================================================="
echo "  Address:       $CLAIM_DROPS_ADDR"
echo "  Owner:         $OWNER"
echo "  Fee bps:       $FEE_BPS"
echo "  Fee collector: $FEE_COLLECTOR"
echo " "

# Machine-readable marker.
echo "DEPLOY_CAPTURE_CLAIM_DROPS_ADDR=$CLAIM_DROPS_ADDR"
echo " "

echo "  Next: set CLAIM_DROPS_CONTRACT.$NETWORK in trippytools"
echo "  (src/utils/claimDrops/config.ts) to $CLAIM_DROPS_ADDR"
echo " "
