use cosmwasm_std::{Binary, Uint128};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::state::{LaunchRecord, LaunchStatus};

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, JsonSchema)]
pub struct InstantiateMsg {
    /// Admin (timelock multisig recommended). Owns the `keeper` and
    /// `forwarder` rotations and the migration entry point.
    pub admin: String,
    /// Tokenfactory subdenom prefix. Must be non-empty and fit alongside a
    /// `u64` decimal suffix inside the 44-char tokenfactory subdenom cap.
    pub subdenom_prefix: String,
    /// Default decimals for every denom this issuer creates. v1 is 18-only.
    pub decimals: u32,
    /// Allowlisted relay key authorized to crank `DeliverToSeeder` /
    /// `RefundFailedLaunch` until the per-launch refund deadline lapses.
    pub keeper: String,
    /// 20-byte bech32 hot key receiving pair-asset from EVM at graduation.
    pub forwarder: String,
    /// Auto-refund window for stuck launches (post-`Registered`, pre-
    /// `Delivered`). After this, anyone can call `RefundFailedLaunch`.
    pub refund_deadline_seconds: u64,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, JsonSchema)]
#[serde(rename_all = "snake_case")]
// `RegisterLaunch` is intentionally a wide message (the whole launch is
// described in one atomic call); the size gap vs. the small admin variants is
// expected and boxing it would only obscure the wire shape.
#[allow(clippy::large_enum_variant)]
pub enum ExecuteMsg {
    /// Keeper-only. Creates the launch denom, mints `total_supply` to self,
    /// ships `evm_supply` to `evm_authority`, pairs the denom to a freshly
    /// auto-deployed `MintBurnBankERC20` via `MsgCreateTokenPair`, and routes
    /// the bundled `create_sink_payload` to the consumer dApp's chosen
    /// `seeder_factory`. All five steps are atomic — any failure reverts the
    /// launch (no partial denom creation, no orphaned mint).
    ///
    /// Gated to [`crate::state::Config::keeper`] (finding C-H1). It was
    /// previously permissionless, which let anyone front-run/squat an
    /// `internal_id` for one create-denom fee and permanently brick a launch.
    /// The launch is keyed by `(evm_authority, internal_id)`, so each EVM
    /// authority gets its own id namespace (no cross-deployment collision).
    RegisterLaunch {
        internal_id: u64,
        /// dApp's EVM authority contract bech32 (lower-20-byte form). Becomes
        /// the recipient of `evm_supply` and the `burn_from_address` at
        /// `DeliverToSeeder` time.
        evm_authority: String,
        /// = `evm_supply` + `cw_held`. Both halves are minted up front to keep
        /// downstream amounts straight.
        total_supply: Uint128,
        /// Initially-circulating amount handed to the EVM authority. The
        /// curve / fair launch / vault uses this on EVM. Must be ≤
        /// `total_supply`. `cw_held = total_supply - evm_supply`.
        evm_supply: Uint128,
        /// Pair asset for the eventual Choice pool. Stored verbatim; the
        /// issuer doesn't act on it directly (Leg C is bank-precompile-fed by
        /// EVM into [`crate::state::Config::forwarder`]).
        pair_denom: String,
        /// Seeder factory bech32 the consumer dApp instantiated (a
        /// `choice_pool_seeder` factory). The issuer forwards
        /// `create_sink_payload` to this contract during registration.
        seeder_factory: String,
        /// Per-launch sink address — caller computes it off-chain as
        /// `instantiate2(seeder_factory, sink_code_id,
        /// salt=encode(this_contract_addr, internal_id))`. Stored verbatim
        /// and used as the `BankMsg::Send` target at `DeliverToSeeder` time.
        seeder_addr: String,
        /// Already-serialized `CreateSink { salt, sink_init }` payload for
        /// the seeder factory. Opaque to the issuer: keeps the
        /// SinkInit/PoolKind/LpDestination surface inside the seeder code-id
        /// where it belongs.
        create_sink_payload: Binary,
        /// Optional `choice_factory` to register the launch denom on via
        /// `AddNativeTokenDecimals`. When set, the issuer atomically chains
        /// a `WasmMsg::Execute` to the given factory with `1` wei of the
        /// fresh launch denom attached — the factory's per-denom verification
        /// reads its own bank balance, so the attached coin doubles as the
        /// "I exist" proof.
        ///
        /// `Some` is the right default for any consumer dApp whose seeder
        /// targets a legacy XYK `choice_factory`. The denom owner (this
        /// issuer) is the only entity authorized to register a
        /// `factory/<this>/...` denom, so deferring this to a separate
        /// keeper tx would not work — see `feedback_inj_bank_precompile_20byte`
        /// for context on why no other actor can sign as the denom owner.
        ///
        /// Side effect: `cw_held` is reduced by 1 wei (the dust delivered to
        /// `choice_factory`). The stored `LaunchRecord.cw_held` reflects the
        /// reduced amount. Caller must therefore ensure
        /// `total_supply - evm_supply >= 1`.
        choice_factory: Option<String>,
        /// Layer A (anti-squat entropy). When `Some`, the per-launch salt is
        /// appended to the subdenom: `{prefix}_{internal_id}_{salt_suffix}`,
        /// making the launch denom unguessable before this tx exists. Must be
        /// ASCII-alphanumeric and short enough that the full subdenom stays
        /// within the 44-char tokenfactory cap. `None` preserves the legacy
        /// `{prefix}_{internal_id}` form. The keeper persists the salt so the
        /// denom round-trips; off-chain consumers must read the denom from the
        /// `register_launch` event rather than recomputing it.
        salt_suffix: Option<String>,
        /// Layer B (anti-squat gate). When `Some`, the issuer chains an
        /// `AuthorizeCreation` at the given CLMM factory reserving the
        /// `(launch_denom, pair_denom, fee)` pool slot for `seeder_addr` (the
        /// sink that runs `CreatePool` at `Settle`). The issuer owns the
        /// `factory/{this}/…` namespace, so it is authorized to reserve. Use
        /// `ttl_seconds = 0` (no expiry) for graduations. `None` skips the
        /// reservation (e.g. legacy XYK graduations).
        clmm_pool_auth: Option<ClmmPoolAuth>,
    },

    /// Keeper-relayed after the EVM authority emits `BootstrapReady(internal_id,
    /// leftover)`. The issuer (as tokenfactory admin with
    /// `allow_admin_burn=true`) burns `leftover` from `evm_authority.bech32`
    /// and sends the retained `cw_held` to `seeder_addr` in the same tx.
    ///
    /// `leftover` is sourced from the EVM event payload so the contract
    /// doesn't need to query the EVM authority's bank balance. Caller
    /// (keeper) is trusted to relay it faithfully; bounded by `leftover ≤
    /// evm_supply`. Public tippable cranking is deferred for v1.
    ///
    /// `evm_authority` identifies the launch namespace (the record is keyed by
    /// `(evm_authority, internal_id)` — finding C-H1). Before sending
    /// `cw_held`, the issuer verifies `seeder_addr` actually holds contract
    /// code, refusing to deliver to a ghost/EOA address (finding C-M3).
    DeliverToSeeder {
        evm_authority: String,
        internal_id: u64,
        leftover: Uint128,
    },

    /// Failure path: burns the CW-side `cw_held` so the launch leaves no
    /// zombie supply on this contract. EVM-side circulating supply cleanup
    /// (refunds to participants, burning of `evm_supply` chunks still at
    /// `evm_authority`, etc.) is the dApp's responsibility via its EVM-side
    /// logic; the issuer doesn't reach into EVM here.
    ///
    /// Callable by `keeper` at any time; after
    /// [`crate::state::Config::refund_deadline_seconds`] past registration the
    /// `admin` may ALSO call it (keeper-outage liveness). It is deliberately
    /// NOT permissionless even post-deadline — a wide-open refund would let
    /// anyone terminally `Refunded` a slow-but-valid launch out from under
    /// graduation. `evm_authority` identifies the launch namespace (the record
    /// is keyed by `(evm_authority, internal_id)` — finding C-H1).
    RefundFailedLaunch {
        evm_authority: String,
        internal_id: u64,
        /// Free-text reason — surfaced as an event attribute for
        /// observability. Not validated.
        reason: String,
    },

    /// Keeper-or-admin, post-`Delivered`: relinquish this contract's
    /// tokenfactory admin over the launch denom by rotating the admin to the
    /// 20-zero-byte burn-address convention via `MsgChangeAdmin` (finding
    /// C-M2). After this the issuer can no longer `MsgMint` new supply or
    /// admin-`MsgBurn`-from holders for the denom.
    ///
    /// NOTE: this renounces the *tokenfactory* admin only. The auto-deployed
    /// `MintBurnBankERC20` owner is the issuer's lower-20-byte EVM address; a
    /// CosmWasm contract cannot sign the EVM tx to renounce that ERC20
    /// ownership, so that step must be performed separately by the issuer's
    /// controller on the EVM side (deployment runbook item).
    RenounceDenomAdmin {
        evm_authority: String,
        internal_id: u64,
    },

    /// Admin-only: step 1 of a two-step admin rotation. Parks `new_admin` as
    /// the pending admin; the live `admin` is unchanged until the pending key
    /// calls [`ExecuteMsg::AcceptAdmin`]. Prevents a typo'd / uncontrolled
    /// target from permanently bricking governance.
    UpdateAdmin { new_admin: String },

    /// Pending-admin-only: step 2 of the rotation. The address parked by
    /// `UpdateAdmin` claims the admin role.
    AcceptAdmin {},

    /// Admin-only: rotate the keeper key.
    UpdateKeeper { new_keeper: String },

    /// Admin-only: rotate the pair-asset forwarder bech32.
    UpdateForwarder { new_forwarder: String },

    /// Admin-only: flip the circuit breaker. While paused, `RegisterLaunch` is
    /// refused; completion + wind-down of in-flight launches stays open.
    SetPaused { paused: bool },

    /// Admin-only: retune the auto-refund liveness window (seconds). Applies
    /// live to every not-yet-refunded launch (the deadline is computed against
    /// the current value).
    UpdateRefundDeadline { new_refund_deadline_seconds: u64 },

    /// Admin-only: retune the default tokenfactory `decimals` for FUTURE
    /// launches (must be `0..=18`). Already-created denoms keep their snapshot
    /// — tokenfactory decimals are immutable once a denom exists.
    UpdateDecimals { new_decimals: u32 },

    /// Admin-only: flip the on-chain `seeder_addr` Instantiate2-derivation
    /// check (P1). When enabled (the default), `RegisterLaunch` re-derives the
    /// sink address from the factory's live `sink_code_id` + salt and rejects a
    /// mismatching `seeder_addr`. Disabling it falls back to the v1 trust model
    /// (the keeper-supplied address, guarded only by `DeliverToSeeder`'s sink
    /// denom-match) — a mainnet escape hatch in case the derivation ever needs
    /// to be turned off without a redeploy. Affects FUTURE registrations only.
    SetVerifySeederDerivation { enabled: bool },
}

/// Layer B reservation parameters for [`ExecuteMsg::RegisterLaunch`]. Carries
/// just enough for the issuer to emit `AuthorizeCreation` at the CLMM factory;
/// the launch denom (token_a) and `pair_denom` (token_b) and the sink
/// (`seeder_addr`, the authorized creator) are already known to the issuer.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, JsonSchema)]
pub struct ClmmPoolAuth {
    /// The consumer dApp's CLMM factory the sink will `CreatePool` on.
    pub clmm_factory: String,
    /// Fee tier (pips) of the graduation pool — must match the sink's
    /// configured `fee_tier`, else the reservation guards the wrong slot.
    pub fee: u32,
    /// Reservation TTL in seconds; `0` means no expiry (the graduation
    /// default — the launch denom is unique, so it can't lapse before Settle).
    pub ttl_seconds: u64,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum QueryMsg {
    Config {},
    /// Look up one launch by its `(evm_authority, internal_id)` key.
    Launch {
        evm_authority: String,
        internal_id: u64,
    },
    /// Paginated listing of a single `evm_authority`'s launches (its `internal_id`
    /// namespace), oldest-first. `start_after` is an `internal_id` within that
    /// authority. Per-authority because `internal_id` is no longer globally
    /// unique (finding C-H1).
    Launches {
        evm_authority: String,
        start_after: Option<u64>,
        limit: Option<u32>,
    },
    /// Reverse lookup: resolve a launch from its fully-qualified bank `denom`
    /// (`factory/<issuer>/<prefix>_<id>_<salt>`). The Layer A salt makes the
    /// denom unguessable, so off-chain consumers can't recompute the
    /// `(evm_authority, internal_id)` key — this is the robust denom→record path
    /// for indexers/integrators. Errors `not_found` if the denom is unknown.
    LaunchByDenom {
        denom: String,
    },
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, JsonSchema)]
pub struct ConfigResponse {
    pub admin: String,
    pub pending_admin: Option<String>,
    pub keeper: String,
    pub subdenom_prefix: String,
    pub decimals: u32,
    pub forwarder: String,
    pub refund_deadline_seconds: u64,
    pub paused: bool,
    /// P1: whether `RegisterLaunch` derives + verifies the sink's Instantiate2
    /// address on-chain (vs. trusting the keeper-supplied `seeder_addr`).
    pub verify_seeder_derivation: bool,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, JsonSchema)]
pub struct LaunchResponse {
    pub internal_id: u64,
    pub status: LaunchStatus,
    pub denom: String,
    pub evm_authority: String,
    pub total_supply: Uint128,
    pub evm_supply: Uint128,
    pub cw_held: Uint128,
    pub pair_denom: String,
    pub seeder_factory: String,
    pub seeder_addr: String,
    pub registered_at: u64,
    pub erc20_address: Option<String>,
    /// Audit field — `Some(addr)` if `RegisterLaunch` chained an
    /// `AddNativeTokenDecimals` call to a `choice_factory`. `None` if the
    /// consumer dApp opted out and is registering decimals separately.
    pub choice_factory: Option<String>,
    /// `true` once `RenounceDenomAdmin` has relinquished this contract's
    /// tokenfactory admin over the denom (finding C-M2).
    pub admin_renounced: bool,
}

impl From<LaunchRecord> for LaunchResponse {
    fn from(r: LaunchRecord) -> Self {
        Self {
            internal_id: r.internal_id,
            status: r.status,
            denom: r.denom,
            evm_authority: r.evm_authority.into_string(),
            total_supply: r.total_supply,
            evm_supply: r.evm_supply,
            cw_held: r.cw_held,
            pair_denom: r.pair_denom,
            seeder_factory: r.seeder_factory.into_string(),
            seeder_addr: r.seeder_addr.into_string(),
            registered_at: r.registered_at,
            erc20_address: r.erc20_address,
            choice_factory: r.choice_factory.map(|a| a.into_string()),
            admin_renounced: r.admin_renounced,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, JsonSchema)]
pub struct LaunchesResponse {
    pub launches: Vec<LaunchResponse>,
}

/// Migration payload. `FromV1` is the legacy on-ramp once a v1 is published;
/// `Patch` is the in-major bump that explicitly cannot touch the (`admin`,
/// `keeper`, `subdenom_prefix`, `decimals`) tuple — same discipline as
/// `choice_zap_lp` to block silent route mutations via `MsgMigrateContract`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum MigrateMsg {
    /// Reserved for the eventual v1 → v2 migration path. Empty until a v1
    /// exists in the wild; defined now so callers can compile against a
    /// stable migrate-msg shape.
    FromV1 {},
    Patch {},
}
