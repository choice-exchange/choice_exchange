use cosmwasm_std::{Decimal, Uint128};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, JsonSchema)]
pub struct InstantiateMsg {
    /// Owner with admin rights (UpdateConfig, Sweep, owner-only Zap). Defaults
    /// to the instantiator.
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
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ExecuteMsg {
    /// Owner-only manual zap. Caller sends one native coin in `info.funds`; it
    /// gets optimally split, swapped, LP'd, and the LP plus any dust go to
    /// `recipient` (falling back to `default_recipient`).
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

    /// Keeper-callable path. Reads the contract's current balance of
    /// `input_denom`, pays the caller `tip_bps` as a tip, and zaps the
    /// remainder into the owner-registered route for that denom. LP + dust
    /// always go to the configured `default_recipient` — no per-call
    /// override, so a compromised keeper cannot redirect funds.
    ///
    /// Caller must be the owner or a registered keeper (see `AddKeeper`).
    /// Errors if no route is registered for `input_denom`, the balance is
    /// below `min_zap_amount`, or `default_recipient` is unset.
    ZapBalance {
        input_denom: String,
        max_spread: Option<Decimal>,
        slippage_tolerance: Option<Decimal>,
        min_lp_out: Option<Uint128>,
        deadline: Option<u64>,
    },

    /// Owner-only: register or overwrite the `input_denom → pair` route used
    /// by `ZapBalance`. Overwriting is allowed (a re-register just replaces
    /// the prior entry).
    RegisterRoute { input_denom: String, pair: String },

    /// Owner-only: remove a route. Subsequent `ZapBalance` calls for that
    /// denom will error.
    UnregisterRoute { input_denom: String },

    /// Owner-only: add a keeper address authorized to call `ZapBalance`.
    AddKeeper { address: String },

    /// Owner-only: revoke a keeper.
    RemoveKeeper { address: String },

    /// Owner-only: update mutable config fields. Pass `None` to leave a field
    /// unchanged; pass empty-string `default_recipient` to clear it.
    UpdateConfig {
        owner: Option<String>,
        default_recipient: Option<String>,
        tip_bps: Option<u16>,
        min_zap_amount: Option<Uint128>,
    },

    /// Owner-only rescue: bank-send the given native denoms held by this contract
    /// to `recipient`. Useful if something gets stuck.
    Sweep {
        recipient: String,
        denoms: Vec<String>,
    },

    /// Internal sub-step. Only callable by the contract itself.
    Callback(CallbackMsg),
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CallbackMsg {
    /// After Swap: deposit the **delta** of A and B that this call produced
    /// (`current_balance - pre_*`) into `pair` with receiver=self. The pre_*
    /// snapshots isolate this zap from any pre-existing contract balance,
    /// which is what makes the user-facing `Zap` path safely permissionless.
    ProvideLiquidity {
        pair: String,
        denom_a: String,
        denom_b: String,
        pre_a: Uint128,
        pre_b: Uint128,
        slippage_tolerance: Decimal,
        deadline: Option<u64>,
    },
    /// After ProvideLiquidity: forward the LP and dust deltas (current minus
    /// snapshot) to recipient. Any pre-existing balance — royalties queued
    /// up before the zap, prior dust, etc. — stays put.
    Sweep {
        recipient: String,
        denom_a: String,
        denom_b: String,
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
        input_denom: String,
        input_amount: Uint128,
    },
    /// Look up the registered pair for an input denom.
    Route { input_denom: String },
    /// List all registered routes.
    Routes {},
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
pub struct RouteResponse {
    pub input_denom: String,
    pub pair: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, JsonSchema)]
pub struct RoutesResponse {
    pub routes: Vec<RouteResponse>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, JsonSchema)]
pub struct KeepersResponse {
    pub keepers: Vec<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, JsonSchema)]
pub struct IsKeeperResponse {
    pub is_keeper: bool,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, JsonSchema)]
pub struct MigrateMsg {}
