use cosmwasm_std::{Decimal, Uint128};
use cw20::Cw20ReceiveMsg;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use choice::asset::AssetInfo;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, JsonSchema)]
pub struct InstantiateMsg {
    /// Owner with admin rights (UpdateConfig, Sweep, AddKeeper/RemoveKeeper).
    /// Defaults to the instantiator.
    pub owner: Option<String>,
    /// Recipient of LP+dust for `ZapBalance`. Also the fallback for `Zap`.
    /// `ZapBalance` errors until this is set.
    pub default_recipient: Option<String>,
    /// Caller tip on `ZapBalance`, in basis points of the input-side balance.
    /// 0 disables the tip. Capped at 100 bps (1%) in code.
    pub tip_bps: Option<u16>,
    /// Minimum input-side balance the keeper path will act on. Below this,
    /// `ZapBalance` errors so keepers stop wasting gas.
    pub min_zap_amount: Option<Uint128>,
    /// Royalty input asset for `ZapBalance`. Immutable post-instantiate.
    pub input: AssetInfo,
    /// Royalty target pair for `ZapBalance`. Immutable post-instantiate.
    pub pair: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ExecuteMsg {
    /// Permissionless manual zap for **native input**. Caller sends one native
    /// coin in `info.funds`; it gets optimally split, swapped, LP'd, and the
    /// LP plus any dust go to `recipient` (defaulting to `info.sender`).
    /// Multi-pair: `pair` is passed per-call, independent of the royalty route
    /// pinned in Config.
    ///
    /// For **CW20 input**, do not call this — use `cw20.Send(zap, amount,
    /// ZapHookMsg::Zap { ... })` on the CW20 contract instead, which routes
    /// into the `Receive` handler below.
    Zap {
        pair: String,
        recipient: Option<String>,
        /// Forwarded to the pair's Swap step. Defaults to 0.5%.
        max_spread: Option<Decimal>,
        /// Forwarded to the pair's ProvideLiquidity step. Defaults to 1%.
        slippage_tolerance: Option<Decimal>,
        /// Optional floor on minted LP. Asserted in the sweep step.
        min_lp_out: Option<Uint128>,
        /// Forwarded to pair Swap and ProvideLiquidity.
        deadline: Option<u64>,
    },

    /// CW20 entry point. Triggered when a user calls `cw20.Send(zap, amount,
    /// ZapHookMsg)` on a CW20 token contract that is one side of the target
    /// pair. The CW20 contract becomes `info.sender`; the original caller is
    /// `cw20_msg.sender`. Multi-pair (pair comes from `ZapHookMsg::Zap.pair`).
    Receive(Cw20ReceiveMsg),

    /// Keeper-callable royalty path. Reads the contract's current balance of
    /// `Config.input`, pays the caller `tip_bps` as a tip, and zaps the
    /// remainder into `Config.pair`. LP + dust go to `default_recipient`.
    ///
    /// Caller must be the owner or a registered keeper (see `AddKeeper`).
    /// Errors if the balance is below `min_zap_amount` or `default_recipient`
    /// is unset.
    ZapBalance {
        max_spread: Option<Decimal>,
        slippage_tolerance: Option<Decimal>,
        min_lp_out: Option<Uint128>,
        deadline: Option<u64>,
    },

    /// Owner-only: add a keeper address authorized to call `ZapBalance`.
    AddKeeper { address: String },

    /// Owner-only: revoke a keeper.
    RemoveKeeper { address: String },

    /// Owner-only: update mutable config fields. Pass `None` to leave a field
    /// unchanged; pass empty-string `default_recipient` to clear it. The
    /// royalty route (`input`, `pair`) is intentionally NOT mutable — to
    /// change it, instantiate a new contract.
    UpdateConfig {
        owner: Option<String>,
        default_recipient: Option<String>,
        tip_bps: Option<u16>,
        min_zap_amount: Option<Uint128>,
    },

    /// Owner-only rescue: forward the given assets (native or CW20) held by
    /// this contract to `recipient`. Useful if something gets stuck.
    Sweep {
        recipient: String,
        assets: Vec<AssetInfo>,
    },

    /// Internal sub-step. Only callable by the contract itself.
    Callback(CallbackMsg),
}

/// Payload of a `cw20.Send(zap, amount, msg)` — the CW20 equivalent of the
/// `Zap` execute variant. Wire-compatible: the CW20 contract delivers the
/// tokens and then forwards this binary into the zap's `Receive` handler.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ZapHookMsg {
    Zap {
        pair: String,
        recipient: Option<String>,
        max_spread: Option<Decimal>,
        slippage_tolerance: Option<Decimal>,
        min_lp_out: Option<Uint128>,
        deadline: Option<u64>,
    },
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CallbackMsg {
    /// After Swap: deposit the **delta** of A and B that this call produced
    /// (`current_balance - pre_*`) into `pair` with receiver=self. The pre_*
    /// snapshots isolate this zap from any pre-existing contract balance,
    /// which is what makes the user-facing `Zap` and `Receive` paths safely
    /// permissionless.
    ///
    /// For CW20 sides, this step also emits `IncreaseAllowance(pair,
    /// deposit, AtHeight(h+1))` before the `ProvideLiquidity` call so the
    /// pair can `TransferFrom` the CW20 deposit. The allowance auto-expires
    /// at the next block height.
    ProvideLiquidity {
        pair: String,
        asset_a: AssetInfo,
        asset_b: AssetInfo,
        pre_a: Uint128,
        pre_b: Uint128,
        /// Forwarded to the pair as the LP-step slippage gate. Defaults are
        /// applied in `plan_zap` so this is always a concrete value here.
        slippage_tolerance: Decimal,
        deadline: Option<u64>,
    },
    /// After ProvideLiquidity: forward the LP and dust deltas (current minus
    /// snapshot) to recipient. Native + LP go via `BankMsg::Send`; CW20 dust
    /// goes via `Cw20::Transfer`. Pre-existing balances stay put.
    Sweep {
        recipient: String,
        asset_a: AssetInfo,
        asset_b: AssetInfo,
        lp_denom: String,
        pre_a: Uint128,
        pre_b: Uint128,
        pre_lp: Uint128,
        min_lp_out: Option<Uint128>,
    },
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum QueryMsg {
    Config {},
    /// Off-chain simulation of an optimal-split swap amount given the current
    /// reserves of `pair`. Does not include slippage from the swap itself.
    SimulateZap {
        pair: String,
        input: AssetInfo,
        input_amount: Uint128,
    },
    /// List all keeper addresses (owner is implicitly allowed and not
    /// included).
    Keepers {},
    /// Cheap auth check for a candidate caller. Returns `true` if `address`
    /// is the owner or a registered keeper.
    IsKeeper { address: String },
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, JsonSchema)]
pub struct ConfigResponse {
    pub owner: String,
    pub default_recipient: Option<String>,
    pub tip_bps: u16,
    pub min_zap_amount: Uint128,
    /// Immutable royalty route input asset (used by `ZapBalance`).
    pub input: AssetInfo,
    /// Immutable royalty route pair address (used by `ZapBalance`).
    pub pair: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, JsonSchema)]
pub struct SimulateZapResponse {
    /// How much of the input should be sold for the other side.
    pub swap_amount: Uint128,
    /// Amount of the other side received after the swap (excluding rounding dust).
    pub expected_return: Uint128,
    /// Amount of the input side that will be deposited as LP.
    pub deposit_input_side: Uint128,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, JsonSchema)]
pub struct KeepersResponse {
    pub keepers: Vec<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, JsonSchema)]
pub struct IsKeeperResponse {
    pub is_keeper: bool,
}

/// Migration payload. Two distinct paths so a v2 → v2 patch cannot
/// accidentally rewrite the immutable `(input, pair)` route by re-using the
/// v1-shaped JSON.
///
/// `FromV1` requires the on-chain cw2 version to be `1.x.x` (legacy ROUTES
/// map config). It pins the new immutable route from the message.
///
/// `Patch` requires the on-chain cw2 version to be `2.x.x`. It bumps
/// CONTRACT_VERSION but never touches Config — so a future v2.y → v2.z
/// migration cannot smuggle a route change in via `MsgMigrateContract`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum MigrateMsg {
    FromV1 { input: AssetInfo, pair: String },
    Patch {},
}
