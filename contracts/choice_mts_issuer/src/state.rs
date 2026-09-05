use cosmwasm_std::{Addr, Uint128};
use cw_storage_plus::{Item, Map};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, JsonSchema)]
pub struct Config {
    /// Admin can rotate `keeper`/`forwarder`, tune `decimals`/
    /// `refund_deadline_seconds`, flip the `paused` breaker, and accept
    /// migrations. Should be a timelock multisig in production. Admin rotation
    /// is itself two-step (`UpdateAdmin` proposes → [`Config::pending_admin`]
    /// → `AcceptAdmin`) so a typo'd / uncontrolled target can't brick
    /// governance.
    pub admin: Addr,
    /// Pending admin for the two-step rotation. `#[serde(default)]` so a config
    /// written by a pre-two-step build deserializes as `None` across a migrate.
    #[serde(default)]
    pub pending_admin: Option<Addr>,
    /// Allowlisted relay key authorized to call `DeliverToSeeder` and
    /// `RefundFailedLaunch`. Rotatable by `admin`. Public tippable cranking is
    /// deferred — gating these on a single keeper keeps the v1 trust model
    /// simple (keeper compromise = DoS only, never fund-theft).
    pub keeper: Addr,
    /// Per-instance subdenom prefix. The launch denom is
    /// `factory/<this>/<prefix>_<internal_id>`. Capped at
    /// [`MAX_SUBDENOM_PREFIX_LEN`](crate::contract::MAX_SUBDENOM_PREFIX_LEN)
    /// characters to leave room for a uint64 suffix inside the tokenfactory
    /// 44-char subdenom limit.
    pub subdenom_prefix: String,
    /// Default tokenfactory metadata decimals baked into each denom at
    /// `RegisterLaunch`. The MTS pair inherits this on the EVM side. v1 is
    /// 18-only (mainnet launches assume 18-dec); `UpdateDecimals` can retune
    /// the default for FUTURE launches within `0..=18` (already-created denoms
    /// keep their snapshot — tokenfactory decimals are immutable post-create).
    pub decimals: u32,
    /// 20-byte bech32 hot account that receives pair-asset from EVM via the
    /// bank precompile and immediately forwards to the seeder. Per the
    /// design's bounded-trust analysis: holds ~1-2 blocks of in-flight
    /// pair-asset. Rotatable by `admin` (a rotation has the same trust shape
    /// as introducing a new keeper).
    ///
    /// OFF-CHAIN COORDINATION ONLY: no execute handler in this contract reads
    /// `forwarder` — it is published on-chain purely so the EVM side / keeper
    /// know where to route Leg C. Rotating it via `UpdateForwarder` updates the
    /// stored value (and emits an event) but changes NO on-chain behaviour of
    /// any in-flight launch.
    pub forwarder: Addr,
    /// Liveness window for `RefundFailedLaunch`: the `keeper` may refund a
    /// stuck launch at any time; after this many seconds past
    /// `LaunchRecord.registered_at` the `admin` may ALSO refund (covering a
    /// keeper outage). It is deliberately NOT permissionless even post-deadline
    /// — a wide-open refund would let anyone terminally `Refunded` a slow-but-
    /// valid launch out from under graduation. Tunable by `admin` via
    /// `UpdateRefundDeadline` (affects all not-yet-refunded launches, since the
    /// deadline is computed live against this value).
    pub refund_deadline_seconds: u64,
    /// Circuit breaker. When `true`, `RegisterLaunch` is refused — the
    /// incident-response halt for a discovered bug in the issuance / seeding
    /// path. Completion + wind-down paths (`DeliverToSeeder`,
    /// `RefundFailedLaunch`, `RenounceDenomAdmin`) stay OPEN so in-flight
    /// launches are never stranded by a pause. `#[serde(default)]` → a
    /// pre-pause config deserializes as `false` (unpaused).
    #[serde(default)]
    pub paused: bool,
    /// P1 (this session): when `true`, `RegisterLaunch` derives the per-launch
    /// sink's `Instantiate2` address on-chain — from the seeder factory's live
    /// `sink_code_id` checksum, the factory as creator, and the salt carried in
    /// the forwarded `CreateSink` payload — and rejects a `seeder_addr` that
    /// doesn't match. This is the only thing that stops a fully-compromised
    /// keeper from pointing `cw_held` (and the CLMM `AuthorizeCreation`
    /// `creator`, i.e. the eventual locked-LP recipient) at a denom-matching
    /// look-alike sink it controls — a case the `DeliverToSeeder` SinkConfig
    /// denom-match guard cannot catch. Default `true`; the `admin` (timelock)
    /// can flip it via `SetVerifySeederDerivation` as a mainnet escape hatch so
    /// an unforeseen chain-version interaction can't brick every launch with no
    /// recourse short of a redeploy. `#[serde(default = …)]` → on for any
    /// config written by a pre-P1 build across a migrate.
    #[serde(default = "default_verify_seeder_derivation")]
    pub verify_seeder_derivation: bool,
}

/// Serde default for [`Config::verify_seeder_derivation`]: secure-by-default
/// (`true`) so a migrated pre-P1 config gains the derivation check rather than
/// silently keeping it off.
pub fn default_verify_seeder_derivation() -> bool {
    true
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum LaunchStatus {
    /// `RegisterLaunch` succeeded: denom created, total_supply minted to
    /// self, `evm_supply` sent to `evm_authority`, `cw_held` retained.
    /// `MsgCreateTokenPair` + seeder factory's `CreateSink` were emitted in
    /// the same tx — if either fails the whole tx reverts, so this state
    /// implies both landed.
    Registered,
    /// `DeliverToSeeder` succeeded: leftover burned from `evm_authority`,
    /// `cw_held` sent to `seeder_addr`. Terminal state on the happy path.
    Delivered,
    /// `RefundFailedLaunch` succeeded: `cw_held` burned (self) AND the unsold
    /// EVM-side supply held by `evm_authority` admin-burned (capped at
    /// `evm_supply`), so no dangling launch-denom supply lingers on either
    /// side. Terminal state on the failure path.
    Refunded,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, JsonSchema)]
pub struct LaunchRecord {
    pub internal_id: u64,
    pub status: LaunchStatus,
    /// Fully-qualified bank denom: `factory/<this>/<prefix>_<internal_id>`.
    pub denom: String,
    /// EVM authority's bech32 (the dApp's launch-side contract address,
    /// lower-20-byte form). Holds `evm_supply` of `denom` until graduation;
    /// `DeliverToSeeder` burns `leftover` from this address.
    pub evm_authority: Addr,
    /// Total minted at registration (= `evm_supply` + `cw_held`). Snapshotted
    /// so the contract can sanity-check delivery amounts.
    pub total_supply: Uint128,
    /// Sent to `evm_authority` at registration; settles curve trading on EVM.
    pub evm_supply: Uint128,
    /// Retained by this contract at registration; sent to `seeder_addr` by
    /// `DeliverToSeeder`. The "graduation reserve."
    pub cw_held: Uint128,
    /// Pair asset for the eventual Choice pool. Pass-through to the seeder.
    /// Not used by this contract beyond storage.
    pub pair_denom: String,
    /// Seeder factory the keeper picked at registration. Not used after
    /// `RegisterLaunch` (the sink address below is the live target).
    pub seeder_factory: Addr,
    /// Per-launch sink address computed (off-chain) as
    /// `instantiate2(seeder_factory, sink_code_id, salt=encode(this, internal_id))`.
    /// `DeliverToSeeder` `BankMsg::Send`s `cw_held` of `denom` here. The
    /// seeder factory is expected to instantiate the sink at exactly this
    /// address; if it doesn't, the funds land at a ghost address and the
    /// keeper must `RefundFailedLaunch` once the deadline passes.
    pub seeder_addr: Addr,
    /// Block timestamp (seconds) at registration. `RefundFailedLaunch` goes
    /// permissionless `refund_deadline_seconds` past this.
    pub registered_at: u64,
    /// Auto-deployed paired ERC20 address, captured by the reply handler on
    /// the `MsgCreateTokenPair` SubMsg. `None` until the reply runs;
    /// always-`Some` once `status == Registered`. Lower-20-byte hex form, with
    /// leading `0x`.
    pub erc20_address: Option<String>,
    /// `Some(addr)` if `RegisterLaunch` chained an `AddNativeTokenDecimals`
    /// call to a `choice_factory`. `None` if the consumer dApp opted out and
    /// is registering decimals separately. Stored for audit / dashboards;
    /// not used to gate any downstream action.
    pub choice_factory: Option<Addr>,
    /// `true` once this denom's tokenfactory admin has been rotated AWAY from
    /// this contract — by `RenounceDenomAdmin` or `HandOverDenomAdminToErc20`,
    /// which since 1.2.0 both target the launch's paired ERC20 — relinquishing
    /// the issuer's `MsgMint` / admin-`MsgBurn`-from powers over the denom.
    /// Audit + a guard against a double-rotation (the second `MsgChangeAdmin`
    /// would revert anyway, but we reject early with a clear error). See finding
    /// C-M2.
    ///
    /// 🔴 It is also the "can this still be remediated?" flag: once the admin
    /// has moved, only the NEW admin can move it again, so a launch that was
    /// rotated to the dead address by a pre-1.2.0 build is permanently
    /// unburnable. That is why the handover resolves the ERC20 address BEFORE
    /// setting this.
    pub admin_renounced: bool,
    /// Audit M-3: snapshot of [`Config::verify_seeder_derivation`] at the moment
    /// this launch was registered — i.e. whether the sink-address (and CLMM
    /// locker) Instantiate2 derivation actually ran and matched here. The flag
    /// is an admin escape hatch that can be toggled off then back on; without
    /// this snapshot `DeliverToSeeder` would happily ship `cw_held` to a
    /// `seeder_addr` that was NEVER derivation-checked (registered during an
    /// off-window) once the flag is flipped back on. `DeliverToSeeder` refuses
    /// to deliver to an unverified record while verification is required.
    /// `#[serde(default = …true)]` so a record written by a pre-M-3 build (which
    /// only ever registered under the secure-default derivation) migrates in as
    /// verified rather than becoming undeliverable.
    #[serde(default = "default_seeder_verified")]
    pub seeder_verified: bool,
}

/// Serde default for [`LaunchRecord::seeder_verified`]: pre-M-3 records were
/// written before the snapshot existed; treat them as verified so a migrate
/// doesn't strand their `DeliverToSeeder`.
pub fn default_seeder_verified() -> bool {
    true
}

pub const CONFIG: Item<Config> = Item::new("config");

/// Per-launch state, keyed by `(evm_authority, internal_id)`.
///
/// The `internal_id` alone is NOT globally unique — a consumer dApp's EVM-side
/// counter resets to 0 on every LaunchpadCore redeploy, so two deployments (or
/// two distinct consumer dApps sharing one issuer) can legitimately present the
/// same `internal_id`. Keying by the launch's `evm_authority` (one per
/// LaunchpadCore deployment) gives each authority its own id namespace, so a
/// redeploy can't collide with a prior instance's records. Combined with the
/// keeper-gate on `RegisterLaunch`, this closes the squat/collision class
/// (finding C-H1). The key is `(&evm_authority, internal_id)`.
pub const LAUNCHES: Map<(&Addr, u64), LaunchRecord> = Map::new("launches");

/// Reverse index: fully-qualified launch `denom` → its `(evm_authority,
/// internal_id)` `LAUNCHES` key. Written once at `RegisterLaunch` (the denom is
/// globally unique — it embeds this issuer's address + the per-authority id +
/// the Layer A salt). Lets indexers/integrators resolve a launch from just its
/// bank denom (the EVM-facing identity is the ERC20, and the salt makes the
/// denom unguessable, so a denom→record lookup is the only robust reverse path).
pub const DENOM_INDEX: Map<&str, (Addr, u64)> = Map::new("denom_index");
