#!/bin/bash
set -euo pipefail

# Instantiate the choice_zap_lp contract.
#
# The contract serves two paths from one instance:
#   1. `Zap` — permissionless user zap. Needs nothing in config; the caller
#      supplies pair, recipient, slippage, etc. per call.
#   2. `ZapBalance` — owner/keeper-managed royalty drain. Needs
#      `default_recipient` set (it refuses to run otherwise) plus owner-
#      registered routes and keeper allowlist post-instantiate.
#
# Default config below is **user-zap-only**: no default_recipient, no tip,
# no min_zap_amount. To wire the royalty path, set DEFAULT_RECIPIENT (and
# optionally TIP_BPS / MIN_ZAP_AMOUNT), then post-deploy run RegisterRoute
# + AddKeeper via injectived for each royalty stream.
#
# Required env var (or positional arg 1):
#   ZAP_LP_CODE_ID     code id of choice_zap_lp.wasm
#
# Optional env vars (with defaults):
#   NETWORK              testnet | mainnet (default: testnet)
#   OWNER                contract owner — UpdateConfig / Register{,Un}Route /
#                        {Add,Remove}Keeper / Sweep authority. Also implicit
#                        ZapBalance caller. Default: $SIGNER_ADDRESS. On
#                        mainnet, override to the governance multisig or the
#                        choice_admin_timelock once stable.
#   DEFAULT_RECIPIENT    LP+dust sink for `ZapBalance`. Required to enable
#                        the royalty path; leave empty for a user-zap-only
#                        deploy. Default: "" (unset).
#   TIP_BPS              keeper tip on `ZapBalance`, in bps of input balance.
#                        Hard-capped at 100 (1%) in the contract. Default: 0.
#   MIN_ZAP_AMOUNT       input-side balance floor below which ZapBalance
#                        no-ops (saves keeper gas). Base units. Default: 0.
#   LABEL                wasm contract label
#                        (default: "Choice Zap LP v1.1.2").
#   ADMIN                wasm admin for THIS contract. Default:
#                        $SIGNER_ADDRESS to keep the deploy mutable. Rotate
#                        to clear-admin / a higher-tier multisig once stable.
#
# Usage:
#   # User-zap-only:
#   NETWORK=testnet ZAP_LP_CODE_ID=125 ./deploy/instantiate_zap_lp.sh
#   ./deploy/instantiate_zap_lp.sh 125
#
#   # Royalty pipeline (mainnet):
#   NETWORK=mainnet ZAP_LP_CODE_ID=125 \
#     OWNER=inj1<multisig> DEFAULT_RECIPIENT=inj1<treasury> TIP_BPS=25 \
#     ./deploy/instantiate_zap_lp.sh

source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

ZAP_LP_CODE_ID="${ZAP_LP_CODE_ID:-${1:-}}"
OWNER="${OWNER:-$SIGNER_ADDRESS}"
DEFAULT_RECIPIENT="${DEFAULT_RECIPIENT:-}"
TIP_BPS="${TIP_BPS:-0}"
MIN_ZAP_AMOUNT="${MIN_ZAP_AMOUNT:-0}"
LABEL="${LABEL:-Choice Zap LP v1.1.2}"
ADMIN="${ADMIN:-$SIGNER_ADDRESS}"

if [ -z "$ZAP_LP_CODE_ID" ]; then
    echo "ERROR: ZAP_LP_CODE_ID is required."
    echo "       Pass via env var or positional arg 1."
    echo "       Run ./deploy/upload_zap_lp_code.sh first to get it."
    exit 1
fi
if ! [[ "$ZAP_LP_CODE_ID" =~ ^[0-9]+$ ]]; then
    echo "ERROR: ZAP_LP_CODE_ID '$ZAP_LP_CODE_ID' is not an integer."; exit 1
fi
if ! [[ "$TIP_BPS" =~ ^[0-9]+$ ]]; then
    echo "ERROR: TIP_BPS '$TIP_BPS' is not an integer."; exit 1
fi
if [ "$TIP_BPS" -gt 100 ]; then
    echo "ERROR: TIP_BPS=$TIP_BPS exceeds the contract cap (100 = 1%)."
    echo "       The contract clamps this at 100 on instantiate."
    exit 1
fi
if ! [[ "$MIN_ZAP_AMOUNT" =~ ^[0-9]+$ ]]; then
    echo "ERROR: MIN_ZAP_AMOUNT '$MIN_ZAP_AMOUNT' is not a non-negative integer."; exit 1
fi

banner "⚡ INSTANTIATE CHOICE ZAP LP"
echo "  Code id:            $ZAP_LP_CODE_ID"
echo "  Owner:              $OWNER"
echo "  Default recipient:  ${DEFAULT_RECIPIENT:-(unset — user-zap only)}"
echo "  Tip (bps):          $TIP_BPS"
echo "  Min zap (base):     $MIN_ZAP_AMOUNT"
echo "  Wasm admin:         $ADMIN"
echo " "

# Build InstantiateMsg. owner is Option<String>; we always set it explicitly
# so the deploy doesn't silently inherit $FROM if the signer changes later.
# default_recipient is Option<String> — omit the field when blank so the
# contract leaves it unset (ZapBalance refuses to run until it's populated
# via UpdateConfig).
DEFAULT_RECIPIENT_JSON=""
if [ -n "$DEFAULT_RECIPIENT" ]; then
    DEFAULT_RECIPIENT_JSON=$(printf '  "default_recipient": "%s",\n' "$DEFAULT_RECIPIENT")
fi

# tip_bps is u16 → JSON number. min_zap_amount is Uint128 → JSON string.
INIT_MSG=$(cat <<EOF
{
  "owner": "$OWNER",
${DEFAULT_RECIPIENT_JSON}  "tip_bps": $TIP_BPS,
  "min_zap_amount": "$MIN_ZAP_AMOUNT"
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
ZAP_LP_ADDR=$(instantiate_contract "$ZAP_LP_CODE_ID" "$INIT_MSG" "$LABEL" "$ADMIN")

echo " "
echo "================================================="
echo "  ✅ ZAP LP INSTANTIATED"
echo "================================================="
echo "  Address:            $ZAP_LP_ADDR"
echo "  Owner:              $OWNER"
echo "  Default recipient:  ${DEFAULT_RECIPIENT:-(unset)}"
echo "  Tip (bps):          $TIP_BPS"
echo "  Min zap (base):     $MIN_ZAP_AMOUNT"
echo "  Wasm admin:         $ADMIN"
echo " "

# Machine-readable marker.
echo "DEPLOY_CAPTURE_ZAP_LP_ADDR=$ZAP_LP_ADDR"
echo " "

echo "  Set in the zap-lp frontend .env:"
if [ "$NETWORK" = "mainnet" ]; then
    echo "      VITE_ZAP_CONTRACT_ADDRESS_MAINNET=\"$ZAP_LP_ADDR\""
else
    echo "      VITE_ZAP_CONTRACT_ADDRESS_TESTNET=\"$ZAP_LP_ADDR\""
fi
echo " "

if [ -n "$DEFAULT_RECIPIENT" ]; then
    echo "  Royalty pipeline next steps (owner-only, per royalty asset):"
    echo "      # Native input:"
    echo "      injectived tx wasm execute $ZAP_LP_ADDR \\"
    echo "          '{\"register_route\":{\"input\":{\"native_token\":{\"denom\":\"<denom>\"}},\"pair\":\"inj1<pair>\"}}' \\"
    echo "          --from=$FROM --chain-id=$CHAIN_ID --fees=$FEES --gas=$GAS --node=$NODE"
    echo " "
    echo "      # CW20 input:"
    echo "      injectived tx wasm execute $ZAP_LP_ADDR \\"
    echo "          '{\"register_route\":{\"input\":{\"token\":{\"contract_addr\":\"inj1<cw20>\"}},\"pair\":\"inj1<pair>\"}}' \\"
    echo "          --from=$FROM --chain-id=$CHAIN_ID --fees=$FEES --gas=$GAS --node=$NODE"
    echo " "
    echo "      injectived tx wasm execute $ZAP_LP_ADDR \\"
    echo "          '{\"add_keeper\":{\"address\":\"inj1<keeper>\"}}' \\"
    echo "          --from=$FROM --chain-id=$CHAIN_ID --fees=$FEES --gas=$GAS --node=$NODE"
    echo " "
    echo "  Then point royalties at $ZAP_LP_ADDR (MsgSend for native, cw20::Transfer"
    echo "  for CW20) and run choice-zap-keeper."
fi
echo " "
