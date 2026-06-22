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
    /// Spawn a locker. Holds a single CLMM position NFT permanently and
    /// exposes only fee collection — there is no decrease/burn/transfer path,
    /// so the seeded principal is locked forever while trading fees stream to
    /// a beneficiary. Instantiated by a `Factory` via Instantiate2 (so its
    /// address is derivable from the launch's `internal_id` before the sink
    /// even exists, which is what lets the sink mint the position straight to
    /// it).
    Locker(LockerInit),
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, JsonSchema)]
pub struct FactoryInit {
    /// Admin can rotate itself and update `sink_code_id`. Should be a
    /// timelock multisig in production. The factory's `choice_factory` is
    /// immutable post-instantiate.
    pub admin: String,
    /// Code-id used by [`ExecuteMsg::CreateSink`] in
    /// [`cosmwasm_std::WasmMsg::Instantiate2`]. Typically equal to this
    /// factory's own code-id (single-binary deploy), but kept as a separate
    /// field so a freshly-audited sink code-id can be rolled out by the
    /// admin without rebuilding the factory.
    pub sink_code_id: u64,
    /// Address of the `choice_factory` (legacy XYK) every `Xyk` sink
    /// instantiated by this factory will call `CreatePair` against.
    /// Per-instance immutable so consumer dApps can audit a single factory as
    /// "targets this DEX deployment, full stop."
    pub choice_factory: String,
    /// Address of the `choice_clmm_factory` every `Clmm` sink will call
    /// `CreatePool` against. `None` on a factory that only ever seeds XYK
    /// pools; a `Clmm` `CreateSink` is rejected unless this is set and matches
    /// the sink's `clmm_factory`. Immutable post-instantiate, same audit
    /// rationale as `choice_factory`.
    pub clmm_factory: Option<String>,
    /// Address of the `choice_clmm_manager` (NFT position manager) every
    /// `Clmm` sink mints liquidity through. Paired with `clmm_factory`: both
    /// must be set (and matched) for a `Clmm` sink. Immutable.
    pub clmm_manager: Option<String>,
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
    /// Which DEX this sink graduates to, and the venue-specific config. `Xyk`
    /// reproduces the legacy constant-product path (create pair, provide
    /// liquidity, route LP per `lp_destination`); `Clmm` creates a
    /// concentrated-liquidity pool at the seed ratio and mints a full-range
    /// position NFT to `position_recipient` (typically a `Locker`). Immutable
    /// per-sink.
    pub pool_kind: PoolKind,
    /// `Refund`'s pair-asset recipient. For SHROOM, this is `LaunchpadCore`
    /// (which performs proportional refunds to curve participants on its
    /// own EVM accounting). For other consumer dApps, anywhere they want
    /// any partial Leg-C deposit to land if settlement fails.
    pub refund_receiver: String,
    /// Seconds-after-instantiate at which `Refund` becomes permissionless.
    /// `Settle` is permissionless from t=0; this gate only opens the
    /// alternative termination path.
    pub deadline_seconds: u64,
    /// Committed seed amounts (audit H-1 / M-1). When set, `Settle` seeds
    /// EXACTLY these amounts at EXACTLY this ratio and sweeps any surplus —
    /// pinning the opening price against the donation-reprice attack and
    /// rejecting premature/partial settlement. The keeper computes them from the
    /// launch's deterministic graduation amounts. `None` ⇒ legacy seed-the-live-
    /// balance behaviour (direct-instantiate debug path only). Both must be set
    /// together; setting only one is rejected at `CreateSink`/instantiate.
    #[serde(default)]
    pub expected_token: Option<Uint128>,
    #[serde(default)]
    pub expected_pair: Option<Uint128>,
}

/// Graduation venue + its config. The factory validates the embedded
/// addresses against its own pinned values at `CreateSink` time, so a sink's
/// stored `pool_kind` always names the exact DEX deployment it will touch.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PoolKind {
    /// Legacy constant-product graduation.
    Xyk {
        /// MUST equal `FactoryConfig.choice_factory`. The XYK factory this
        /// sink calls `CreatePair` against.
        choice_factory: String,
        /// What happens to the LP minted by the initial `ProvideLiquidity`:
        /// `Burn` (permanently locked floor) or `SendTo` (treasury / farm).
        lp_destination: LpDestination,
    },
    /// Concentrated-liquidity graduation. `Settle` creates a CLMM pool at the
    /// seed ratio, then mints a single full-range position NFT (via the
    /// manager) to `position_recipient`. Pairing a `Locker` recipient with a
    /// pool whose principal is never decreased = permanently locked liquidity
    /// that still earns fees.
    Clmm {
        /// MUST equal `FactoryConfig.clmm_factory`. The CLMM factory this sink
        /// calls `CreatePool` against.
        clmm_factory: String,
        /// MUST equal `FactoryConfig.clmm_manager`. The NFT position manager
        /// this sink mints liquidity through.
        clmm_manager: String,
        /// Fee tier (pips) for the pool — must be an enabled tier on the CLMM
        /// factory (e.g. 100/500/3000/10000). Decides the tick spacing the
        /// full-range bounds snap to.
        fee_tier: u32,
        /// Owner of the minted position NFT. Usually a `Locker` whose address
        /// the launch derived deterministically; any address that can later
        /// call `manager.Collect` works.
        position_recipient: String,
        /// Dynamic-fee ceiling as a multiple of `fee_tier`, forwarded
        /// verbatim to the CLMM factory's `CreatePool` (valid 2..=10 there).
        /// `None` = factory default (2x). Graduation pools typically want a
        /// high multiple (e.g. 10x) so the locked position's volatility fee
        /// can actually price a post-graduation frenzy.
        max_fee_multiple: Option<u32>,
    },
}

/// Locker role config. The locker owns a CLMM position NFT and can only
/// collect its fees — never withdraw principal. Collected fees are split
/// between a `treasury` leg and a `creator` leg per `creator_fee_share_bps`,
/// mirroring the launch's bonding-curve fee split.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, JsonSchema)]
pub struct LockerInit {
    /// The `choice_clmm_manager` the held position NFT lives in. `CollectFees`
    /// calls `Collect` here; the locker must be the NFT owner.
    pub manager: String,
    /// Treasury leg of the fee split — gets `10_000 - creator_fee_share_bps`
    /// of every collected fee. Rotatable by `admin`.
    pub treasury: String,
    /// Creator leg — gets `creator_fee_share_bps` of every collected fee.
    /// Immutable for the life of the locker (never rotated). The keeper sets
    /// this to the launch creator's bech32 at graduation.
    pub creator: String,
    /// Creator's share of each collected fee, in bps of the fee (`≤ 10_000`).
    /// The keeper sources this from the launch's `creatorFeeShareBps` snapshot
    /// on `LaunchpadCore` so the graduated split equals the bonding-curve one.
    pub creator_fee_share_bps: u16,
    /// Optional admin allowed to rotate the `treasury` leg. `None` makes the
    /// treasury immutable too — a fully trust-minimized locker. The creator
    /// leg is immutable regardless of this field.
    pub admin: Option<String>,
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

    /// **Factory-only.** Spawns a `Locker` at `instantiate2(this_factory,
    /// sink_code_id, salt)` (the seeder binary serves all three roles, so the
    /// same code-id is used). Permissionless, same security model as
    /// `CreateSink`: the locker is addressable only by the salt the caller
    /// supplied. The keeper computes the locker address from a launch-specific
    /// salt and passes it as a `Clmm` sink's `position_recipient`, so the
    /// position NFT mints straight into it.
    CreateLocker {
        salt: Binary,
        locker_init: LockerInit,
    },

    /// **Sink-only, permissionless.** Single-shot. Reads the sink's own bank
    /// balances of `token_denom` and `pair_denom`, then submits
    /// `factory.CreatePair` followed by `pair.ProvideLiquidity` and routes the
    /// minted LP per `lp_destination`. Atomic across all sub-messages. The
    /// cranker receives no tip — the entire balance seeds the pool.
    ///
    /// Prerequisites the caller MUST ensure:
    ///   * Both denoms are pre-registered on the target `choice_factory` via
    ///     `AddNativeTokenDecimals`. For the launch denom, `choice_mts_issuer`
    ///     handles this in its `RegisterLaunch` flow (the issuer is the
    ///     denom-factory admin and can sign the registration). For the pair
    ///     denom, this is the consumer dApp's responsibility — usually a
    ///     no-op because the pair denom (SHROOM, INJ, etc.) is already
    ///     registered from existing pools.
    ///   * The create-pair fee rides in `info.funds` on the `Settle` tx
    ///     itself (NOT pre-funded into the sink balance): an `Xyk` sink must
    ///     attach EXACTLY the live tokenfactory denom-creation fee (queried at
    ///     runtime; the chain debits it to mint `factory/<pair>/lp`), while a
    ///     `Clmm` sink must attach NOTHING — pool creation is free and any
    ///     attached funds are rejected (`UnexpectedFundsForClmmSettle`).
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

    /// **Factory admin.** Step 1 of a two-step admin rotation: park
    /// `new_admin` as the pending admin. It does NOT take effect until the
    /// pending key calls [`ExecuteMsg::AcceptAdmin`], so a rotation to a
    /// typo'd / uncontrolled address cannot brick governance. Passing the
    /// current admin (or re-calling) overwrites any prior pending nomination.
    UpdateAdmin { new_admin: String },

    /// **Pending admin only.** Step 2 of the rotation: the address parked by
    /// `UpdateAdmin` claims the admin role. Errors if there is no pending
    /// nomination or the caller is not it.
    AcceptAdmin {},

    /// **Factory admin.** Swap in a new sink code-id (presumably a v2 audited
    /// build). Future `CreateSink` calls instantiate against the new code-id;
    /// existing sinks are unaffected — they were instantiated against the
    /// old one and are immutable post-instantiate.
    UpdateSinkCodeId { new_sink_code_id: u64 },

    /// **Factory admin.** Flip the factory circuit breaker. While `paused`,
    /// `CreateSink` / `CreateLocker` are refused and every sink that consults
    /// this factory freezes its `Settle`. The incident-response halt for a
    /// discovered DEX / seeding bug; lift it to resume.
    SetPaused { paused: bool },

    /// **Factory admin.** Re-point the XYK `choice_factory` after an XYK
    /// redeploy. Affects only FUTURE sinks — already-spawned sinks carry their
    /// own snapshot. Avoids a full factory redeploy when the DEX moves.
    UpdateChoiceFactory { new_choice_factory: String },

    /// **Factory admin.** Re-point the CLMM factory + manager after a CLMM
    /// redeploy (the addresses most likely to churn during active
    /// development). All-or-nothing: pass BOTH to enable/repoint CLMM
    /// graduation, or BOTH `None` to disable it. Future sinks only.
    UpdateClmmAddresses {
        clmm_factory: Option<String>,
        clmm_manager: Option<String>,
    },

    /// **Sink-only, factory-admin-gated.** Emergency termination of a
    /// `Pending` sink that cannot (or should not) settle — e.g. a seed ratio
    /// that trips `SeedRatioOutOfRange`, an unsupported fee tier, or a sink
    /// caught up in a paused incident. Identical fund routing to `Refund`
    /// (token → issuer, pair → refund_receiver) but BYPASSES the
    /// `deadline_seconds` wait. Authenticated against the parent factory's
    /// `admin` (looked up via [`crate::state::SinkConfig::factory`]).
    ForceRefund {},

    /// **Sink internal sub-step.** Only callable by the sink itself; gated by
    /// `info.sender == env.contract.address`. Wraps the post-`CreatePair`
    /// chain so the sink can query the new pair address out of factory
    /// storage between steps.
    Callback(CallbackMsg),

    /// **Locker-only, permissionless.** Collects accrued fees on the held
    /// position NFT(s) into the locker itself, then chains a
    /// `Callback(DistributeFees)` that splits every collected denom between the
    /// `treasury` and `creator` legs per `creator_fee_share_bps`. With
    /// `token_id` `None`, collects every NFT the locker owns (first page of the
    /// manager's `Tokens` enumeration); with `Some`, collects exactly that one.
    /// There is intentionally no path here to decrease or burn liquidity —
    /// principal stays locked.
    CollectFees { token_id: Option<String> },

    /// **Locker admin.** Rotate the `treasury` leg of the fee split. Errors if
    /// the locker was instantiated with no admin (immutable treasury). The
    /// `creator` leg is immutable and cannot be rotated through any path.
    UpdateTreasury { new_treasury: String },
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

    /// Final step of the `Clmm` `Settle`: after `manager.MintPosition` has run
    /// and refunded any one-sided surplus back to this sink, sweep whatever
    /// `token_denom` / `pair_denom` dust remains to `refund_receiver` so no
    /// value strands in a terminal sink. A no-op (no messages) when nothing
    /// is left.
    SweepDust {},

    /// **Locker step 2 of `CollectFees`.** After `manager.Collect` has routed
    /// the position's accrued fees into this locker, split every nonzero bank
    /// balance between the `treasury` and `creator` legs per
    /// `creator_fee_share_bps`. Treasury absorbs the rounding remainder so no
    /// dust strands. Denom-agnostic: handles both pool tokens (and any stray
    /// balance) without needing to know the position's token0/token1 up front.
    DistributeFees {},
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
    /// plus, if settled, the resulting venue artifacts — `pair_addr` /
    /// `lp_minted` for XYK, `pool_addr` / `position_token_id` for CLMM.
    SinkState {},
    /// Targeted query: errors with `WrongRole` unless invoked on a locker.
    LockerConfig {},
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RoleResponse {
    Factory(FactoryConfigResponse),
    // Boxed to keep this enum small — the inline `Sink` payload is ~3x the
    // next-largest variant (clippy::large_enum_variant). `Box` is transparent
    // to serde + schemars, so the JSON wire format and schema are unchanged.
    Sink {
        config: Box<SinkConfigResponse>,
        state: Box<SinkStateResponse>,
    },
    Locker(LockerConfigResponse),
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, JsonSchema)]
pub struct FactoryConfigResponse {
    pub admin: String,
    pub pending_admin: Option<String>,
    pub sink_code_id: u64,
    pub choice_factory: String,
    pub clmm_factory: Option<String>,
    pub clmm_manager: Option<String>,
    pub paused: bool,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, JsonSchema)]
pub struct SinkConfigResponse {
    pub factory: Option<String>,
    pub issuer: String,
    pub token_denom: String,
    pub pair_denom: String,
    pub token_decimals: u8,
    pub pair_decimals: u8,
    pub pool_kind: PoolKind,
    pub refund_receiver: String,
    pub deadline_seconds: u64,
    pub instantiated_at: u64,
    #[serde(default)]
    pub expected_token: Option<Uint128>,
    #[serde(default)]
    pub expected_pair: Option<Uint128>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, JsonSchema)]
pub struct LockerConfigResponse {
    pub manager: String,
    pub treasury: String,
    pub creator: String,
    pub creator_fee_share_bps: u16,
    pub admin: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, JsonSchema)]
pub struct SinkStateResponse {
    pub status: SinkStatus,
    /// XYK-only: the pair created at settle. CLMM settles report `pool_addr`.
    pub pair_addr: Option<String>,
    /// XYK-only: LP minted at settle.
    pub lp_minted: Option<Uint128>,
    /// OBSERVABILITY: CLMM pool created at settle — `None` for XYK / pre-settle.
    #[serde(default)]
    pub pool_addr: Option<String>,
    /// OBSERVABILITY: position NFT `token_id` minted to the locker at CLMM
    /// settle — `None` for XYK / pre-settle.
    #[serde(default)]
    pub position_token_id: Option<String>,
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
