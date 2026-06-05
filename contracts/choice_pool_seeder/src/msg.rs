use cosmwasm_std::{Binary, Uint128};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::state::SinkStatus;

/// Two-role InstantiateMsg. Same code-id binary serves both the factory and
/// the per-launch sink — the role is fixed at instantiate time and the
/// dispatch tables in `contract::execute` / `contract::query` reject
/// cross-role calls.
///
/// Why one code-id and not two: every sink is a separate contract
/// instantiated by the factory via [`cosmwasm_std::WasmMsg::Instantiate2`], so
/// the factory needs a `sink_code_id` to point at. Sharing the code-id keeps
/// audits and deployments single-target; isolation is provided by the
/// per-instance config + the [`crate::state::ROLE`] guard inside every
/// handler.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum InstantiateMsg {
    /// Spawn a factory. The dApp instantiates one per `choice_mts_issuer`
    /// instance (or one per network) and points its issuer at the resulting
    /// address. Holds no funds; only metadata.
    Factory(FactoryInit),
    /// Spawn a sink. Always instantiated by a `Factory` via Instantiate2 —
    /// the direct-instantiate path is supported only as a debug aid; in
    /// production it would create a sink that no `choice_mts_issuer` can
    /// match against (because the deterministic address derivation depends on
    /// the factory being the creator).
    Sink(SinkInit),
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, JsonSchema)]
pub struct FactoryInit {
    /// Admin can rotate itself and update `sink_code_id`. Should be a
    /// timelock multisig in production. The factory's `choice_factory` and
    /// `max_tip_bps` are immutable post-instantiate.
    pub admin: String,
    /// Code-id used by [`ExecuteMsg::CreateSink`] in
    /// [`cosmwasm_std::WasmMsg::Instantiate2`]. Typically equal to this
    /// factory's own code-id (single-binary deploy), but kept as a separate
    /// field so a freshly-audited sink code-id can be rolled out by the
    /// admin without rebuilding the factory.
    pub sink_code_id: u64,
    /// Address of the `choice_factory` (legacy XYK) every sink instantiated
    /// by this factory will call `CreatePair` against. Per-instance immutable
    /// so consumer dApps can audit a single factory as "targets this DEX
    /// deployment, full stop."
    pub choice_factory: String,
    /// Hard cap on `SinkInit.tip_bps`. Belt-and-suspenders against a
    /// misconfigured (or malicious) consumer dApp draining the pair-asset
    /// side to its own tip recipient.
    pub max_tip_bps: u16,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, JsonSchema)]
pub struct SinkInit {
    /// Issuer that's about to feed Leg B into this sink (bech32 of a
    /// [`choice_mts_issuer`](../../choice_mts_issuer) instance). Stored for
    /// `Refund`-time provenance: any leftover launch-denom balance goes back
    /// here to be burned.
    pub issuer: String,
    /// Tokenfactory launch denom (`factory/<issuer>/<prefix>_<id>`). The
    /// sink's bank balance of this denom comes from issuer Leg B
    /// (`BankMsg::Send` from the issuer after `DeliverToSeeder`).
    pub token_denom: String,
    /// Pair asset for the eventual Choice pool. The sink's bank balance of
    /// this denom comes from EVM Leg C — the forwarder bech32 receives
    /// pair-asset from the bank precompile and immediately forwards to this
    /// sink's address.
    pub pair_denom: String,
    /// Decimals for `token_denom`. Pass-through onto the Choice pair via the
    /// factory's `query_decimals` flow; only used for observability today
    /// since the factory queries decimals from its own `NATIVE_TOKEN_DECIMALS`
    /// map (pre-registered out of band by the consumer dApp's issuer).
    pub token_decimals: u8,
    /// Decimals for `pair_denom`. Same note as above.
    pub pair_decimals: u8,
    /// What happens to the LP minted by the initial `ProvideLiquidity`.
    /// Immutable per-sink — auditors can decide once whether a given
    /// launchpad's pools are permanently locked (`Burn`) or routable to a
    /// treasury (`SendTo`). `Lock { until, beneficiary }` is deferred to v2.
    pub lp_destination: LpDestination,
    /// `Refund`'s pair-asset recipient. For SHROOM, this is `LaunchpadCore`
    /// (which performs proportional refunds to curve participants on its
    /// own EVM accounting). For other consumer dApps, anywhere they want
    /// any partial Leg-C deposit to land if settlement fails.
    pub refund_receiver: String,
    /// Seconds-after-instantiate at which `Refund` becomes permissionless.
    /// `Settle` is permissionless from t=0; this gate only opens the
    /// alternative termination path.
    pub deadline_seconds: u64,
    /// Caller tip on permissionless `Settle`, in basis points of the
    /// pair-asset balance at settle time. 0 disables the tip. Capped at
    /// `FactoryConfig.max_tip_bps`.
    pub tip_bps: u16,
    /// MUST equal `FactoryConfig.choice_factory`. Carried explicitly so
    /// auditors reading a sink's stored config see the whole route in one
    /// place; the factory rejects mismatches at `CreateSink` time.
    pub choice_factory: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum LpDestination {
    /// `BankMsg::Burn` the freshly-minted LP. Once burned, the pool's
    /// initial liquidity is permanently unwithdrawable — the launch-token
    /// holders trade against a floor of locked liquidity. SHROOM's v1
    /// default.
    Burn,
    /// `BankMsg::Send` the LP to an address. Useful for treasuries, future
    /// locker contracts, or LP-incentive farms. The address is validated at
    /// `CreateSink` time.
    SendTo(String),
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ExecuteMsg {
    /// **Factory-only.** Spawns a per-launch sink at
    /// `instantiate2(this_factory, sink_code_id, salt)`. The
    /// `choice_mts_issuer.RegisterLaunch` flow forwards an opaque
    /// `create_sink_payload` Binary to this handler; that payload is exactly
    /// `to_json_binary(&ExecuteMsg::CreateSink { salt, sink_init })`.
    ///
    /// Permissionless — the security model is that the resulting sink is
    /// addressable only by the salt the caller supplied. A caller who
    /// instantiates a sink with a salt no issuer ever computed creates a
    /// sink that no Leg B will ever feed; it self-destructs via `Refund`
    /// past the deadline.
    CreateSink {
        /// Caller-supplied salt for [`cosmwasm_std::WasmMsg::Instantiate2`].
        /// Convention from `choice_mts_issuer`: `encode(issuer_addr,
        /// internal_id)`. The factory does not interpret it; addresses are
        /// derived deterministically downstream.
        salt: Binary,
        sink_init: SinkInit,
    },

    /// **Sink-only, permissionless.** Single-shot. Reads the sink's own bank
    /// balances of `token_denom` and `pair_denom`, deducts `tip_bps` of
    /// `pair_denom` to the caller, then submits `factory.CreatePair` followed
    /// by `pair.ProvideLiquidity` and routes the minted LP per
    /// `lp_destination`. Atomic across all sub-messages.
    ///
    /// Prerequisites the caller MUST ensure:
    ///   * Both denoms are pre-registered on the target `choice_factory` via
    ///     `AddNativeTokenDecimals`. For the launch denom, `choice_mts_issuer`
    ///     handles this in its `RegisterLaunch` flow (the issuer is the
    ///     denom-factory admin and can sign the registration). For the pair
    ///     denom, this is the consumer dApp's responsibility — usually a
    ///     no-op because the pair denom (SHROOM, INJ, etc.) is already
    ///     registered from existing pools.
    ///   * The sink's bank balance includes at least the tokenfactory
    ///     `create_pair` fee in INJ (queried by `Settle` at runtime). The
    ///     keeper or settlement cranker pre-funds this — typically alongside
    ///     the Leg C pair-asset forward.
    Settle {},

    /// **Sink-only, permissionless after `deadline_seconds`.** Routes:
    ///
    ///   * `token_denom` back to the configured `issuer` (which has
    ///     `RefundFailedLaunch` to burn it cleanly, leaving no zombie supply
    ///     on the CW side).
    ///   * `pair_denom` to `refund_receiver` (the consumer dApp's EVM-side
    ///     refund authority).
    ///
    /// Single-shot, mutually exclusive with `Settle`.
    Refund {},

    /// **Factory admin.** Rotate the admin key. Both old and new admin must
    /// validate as bech32; otherwise unchecked (the timelock at the admin
    /// boundary provides the time delay).
    UpdateAdmin {
        new_admin: String,
    },

    /// **Factory admin.** Swap in a new sink code-id (presumably a v2 audited
    /// build). Future `CreateSink` calls instantiate against the new code-id;
    /// existing sinks are unaffected — they were instantiated against the
    /// old one and are immutable post-instantiate.
    UpdateSinkCodeId {
        new_sink_code_id: u64,
    },

    /// **Sink internal sub-step.** Only callable by the sink itself; gated by
    /// `info.sender == env.contract.address`. Wraps the post-`CreatePair`
    /// chain so the sink can query the new pair address out of factory
    /// storage between steps.
    Callback(CallbackMsg),
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CallbackMsg {
    /// Step 2 of `Settle`: queries `factory.Pair { asset_infos }` to get the
    /// freshly-created pair address, then emits `pair.ProvideLiquidity` with
    /// `token_amount` + `pair_amount` attached as funds.
    ProvideLiquidity {
        token_amount: Uint128,
        pair_amount: Uint128,
    },
    /// Step 3 of `Settle`: queries the sink's bank balance of the pair's
    /// `liquidity_token` denom (which is `factory/<pair_addr>/lp`) and
    /// routes it per `lp_destination`. Asserts `lp_minted > 0`; defends
    /// against a bug where the pair's `share == 0` branch silently produced
    /// no LP.
    DistributeLp {},
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum QueryMsg {
    /// Whichever role this instance has. Returns [`RoleResponse::Factory`]
    /// or [`RoleResponse::Sink`] with the full config inline — clients
    /// don't need to know the role up front.
    Role {},
    /// Targeted query: errors with `WrongRole` if invoked on a sink.
    FactoryConfig {},
    /// Targeted query: errors with `WrongRole` if invoked on a factory.
    SinkConfig {},
    /// Sink-only mutable state: status (`Pending` / `Settled` / `Refunded`)
    /// plus, if settled, the resulting pair address and LP minted amount.
    SinkState {},
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RoleResponse {
    Factory(FactoryConfigResponse),
    Sink {
        config: SinkConfigResponse,
        state: SinkStateResponse,
    },
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, JsonSchema)]
pub struct FactoryConfigResponse {
    pub admin: String,
    pub sink_code_id: u64,
    pub choice_factory: String,
    pub max_tip_bps: u16,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, JsonSchema)]
pub struct SinkConfigResponse {
    pub issuer: String,
    pub token_denom: String,
    pub pair_denom: String,
    pub token_decimals: u8,
    pub pair_decimals: u8,
    pub lp_destination: LpDestination,
    pub refund_receiver: String,
    pub deadline_seconds: u64,
    pub instantiated_at: u64,
    pub tip_bps: u16,
    pub choice_factory: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, JsonSchema)]
pub struct SinkStateResponse {
    pub status: SinkStatus,
    pub pair_addr: Option<String>,
    pub lp_minted: Option<Uint128>,
}

/// Migration payload. Same `FromV1` / `Patch` split as
/// [`choice_zap_lp`'s](../../choice_zap_lp) — `Patch` is constrained to
/// in-major bumps so an admin can't silently rewrite `choice_factory` or
/// `sink_code_id` via a v2 → v2 `MsgMigrateContract` with a v1-shaped
/// payload. `FromV1` is wired so callers can compile against a stable
/// migrate-msg shape before a real v1 → v2 schema delta exists.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum MigrateMsg {
    FromV1 {},
    Patch {},
}
