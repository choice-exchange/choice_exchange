use cosmwasm_std::{Addr, Uint128};
use cw_storage_plus::{Item, Map};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, JsonSchema)]
pub struct Config {
    /// Admin can rotate `keeper` and `forwarder` and accept migrations. Should
    /// be a timelock multisig in production. Rotation of `admin` itself goes
    /// through the same timelock path.
    pub admin: Addr,
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
    /// Default tokenfactory metadata decimals. The MTS pair inherits this on
    /// the EVM side. v1 is 18-only (mainnet launches assume 18-dec); refer
    /// design §10 item 6.
    pub decimals: u32,
    /// 20-byte bech32 hot account that receives pair-asset from EVM via the
    /// bank precompile and immediately forwards to the seeder. Per the
    /// design's bounded-trust analysis: holds ~1-2 blocks of in-flight
    /// pair-asset. Rotatable by `admin` (a rotation has the same trust shape
    /// as introducing a new keeper).
    pub forwarder: Addr,
    /// `RefundFailedLaunch` is auto-callable by anyone after this many
    /// seconds past `LaunchRecord.registered_at`, in case the keeper goes
    /// down between `BootstrapReady`/`BootstrapFailed` and the corresponding
    /// CW relay. Until then, only the keeper can refund.
    pub refund_deadline_seconds: u64,
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
    /// `true` once `RenounceDenomAdmin` has rotated this denom's tokenfactory
    /// admin to the burn-address convention, relinquishing the issuer's
    /// `MsgMint` / admin-`MsgBurn`-from powers over the denom. Audit + a guard
    /// against a double-renounce (the second `MsgChangeAdmin` would revert
    /// anyway, but we reject early with a clear error). See finding C-M2.
    pub admin_renounced: bool,
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
