use cosmwasm_std::{Addr, Uint128};
use cw_storage_plus::Item;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::msg::LpDestination;

/// Tagged at instantiate, immutable. Every handler in
/// [`crate::contract::execute`] / [`crate::contract::query`] dispatches off
/// this — wrong-role calls return [`crate::error::ContractError::WrongRole`].
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Factory,
    Sink,
    Locker,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, JsonSchema)]
pub struct FactoryConfig {
    pub admin: Addr,
    /// Pending admin for the two-step rotation. `UpdateAdmin` parks the
    /// proposed key here; `AcceptAdmin` (called BY that key) promotes it. A
    /// one-step rotation to a typo'd / uncontrolled address would brick
    /// factory governance permanently — the handshake makes that
    /// unreachable. `#[serde(default)]` so a config written by a pre-two-step
    /// build deserializes as `None` across a migrate.
    #[serde(default)]
    pub pending_admin: Option<Addr>,
    /// Code-id used by `Instantiate2` for both sinks and lockers (the seeder
    /// binary serves every role). Mutable via `UpdateSinkCodeId`.
    /// Single-binary deploys set this == own code-id at instantiate; the
    /// admin can repoint to a freshly-audited build later.
    pub sink_code_id: u64,
    /// XYK DEX deployment this factory pins for `Xyk` sinks. Mutable via
    /// `UpdateChoiceFactory` so an XYK redeploy can be re-pointed without
    /// redeploying the factory. Only affects FUTURE sinks — already-spawned
    /// sinks carry their own snapshot in `PoolKindStored`.
    pub choice_factory: Addr,
    /// The CLMM factory `Clmm` sinks create pools on. `None` disables CLMM
    /// graduation. Mutable via `UpdateClmmAddresses` (set together with
    /// `clmm_manager`) so a CLMM redeploy can be re-pointed. Future sinks only.
    pub clmm_factory: Option<Addr>,
    /// The CLMM NFT position manager `Clmm` sinks mint through. Set/cleared
    /// together with `clmm_factory` via `UpdateClmmAddresses`. Future sinks only.
    pub clmm_manager: Option<Addr>,
    /// Circuit breaker. When `true`, `CreateSink` / `CreateLocker` are refused
    /// and any sink whose `Settle` consults this factory (via
    /// [`SinkConfig::factory`]) is frozen — the incident-response halt for a
    /// discovered DEX/seeding bug. Toggled by `SetPaused`. `#[serde(default)]`
    /// so a pre-pause config deserializes as `false` (unpaused).
    #[serde(default)]
    pub paused: bool,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, JsonSchema)]
pub struct SinkConfig {
    /// Factory that instantiated this sink (`info.sender` at instantiate).
    /// Lets `Settle` consult the factory's `paused` circuit breaker and
    /// `ForceRefund` authenticate against the factory `admin` — without the
    /// sink needing its own admin field. `None` only on the direct-instantiate
    /// debug path (no parent factory to consult); `#[serde(default)]` so a
    /// pre-existing stored sink deserializes as `None`.
    #[serde(default)]
    pub factory: Option<Addr>,
    pub issuer: Addr,
    pub token_denom: String,
    pub pair_denom: String,
    pub token_decimals: u8,
    pub pair_decimals: u8,
    pub pool_kind: PoolKindStored,
    pub refund_receiver: Addr,
    pub deadline_seconds: u64,
    /// Wall-clock seconds at instantiate. `Refund`'s permissionless gate
    /// opens at `instantiated_at + deadline_seconds`.
    pub instantiated_at: u64,
}

/// Address-validated mirror of [`crate::msg::PoolKind`].
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PoolKindStored {
    Xyk {
        choice_factory: Addr,
        lp_destination: LpDestinationStored,
    },
    Clmm {
        clmm_factory: Addr,
        clmm_manager: Addr,
        fee_tier: u32,
        position_recipient: Addr,
        /// Forwarded to the CLMM factory's `CreatePool`; `None` = factory
        /// default (2x). Pre-existing stored sinks deserialize as `None`.
        max_fee_multiple: Option<u32>,
    },
}

impl From<&PoolKindStored> for crate::msg::PoolKind {
    fn from(s: &PoolKindStored) -> Self {
        match s {
            PoolKindStored::Xyk {
                choice_factory,
                lp_destination,
            } => crate::msg::PoolKind::Xyk {
                choice_factory: choice_factory.to_string(),
                lp_destination: lp_destination.into(),
            },
            PoolKindStored::Clmm {
                clmm_factory,
                clmm_manager,
                fee_tier,
                position_recipient,
                max_fee_multiple,
            } => crate::msg::PoolKind::Clmm {
                clmm_factory: clmm_factory.to_string(),
                clmm_manager: clmm_manager.to_string(),
                fee_tier: *fee_tier,
                position_recipient: position_recipient.to_string(),
                max_fee_multiple: *max_fee_multiple,
            },
        }
    }
}

/// Locker role config — owns a CLMM position NFT, collects fees and splits
/// them between a `treasury` leg and a `creator` leg, never withdraws
/// principal. The split mirrors the launch's `creatorFeeShareBps` snapshot on
/// `LaunchpadCore` so graduated-pool fees follow the same incentive as the
/// bonding-curve trading fee.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, JsonSchema)]
pub struct LockerConfig {
    pub manager: Addr,
    /// Treasury leg — receives `10_000 - creator_fee_share_bps` of every
    /// collected fee. Rotatable by `admin`.
    pub treasury: Addr,
    /// Creator leg — receives `creator_fee_share_bps` of every collected fee.
    /// IMMUTABLE for the life of the locker (never rotated, even by admin) so
    /// the creator holds a permanent on-chain guarantee of their cut.
    pub creator: Addr,
    /// Creator's share of each collected fee, in basis points of the fee.
    /// Mirrors the launch's `creatorFeeShareBps`. `≤ 10_000`.
    pub creator_fee_share_bps: u16,
    /// `None` ⇒ treasury leg immutable too (fully trust-minimized locker). The
    /// creator leg is immutable regardless.
    pub admin: Option<Addr>,
}

/// Address-validated variant of [`crate::msg::LpDestination`] — kept separate
/// so JSON-serialized state files always carry the bech32 form the chain
/// approved at instantiate time. `From` conversions in both directions live
/// inline below.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum LpDestinationStored {
    Burn,
    SendTo(Addr),
}

impl From<&LpDestinationStored> for LpDestination {
    fn from(s: &LpDestinationStored) -> Self {
        match s {
            LpDestinationStored::Burn => LpDestination::Burn,
            LpDestinationStored::SendTo(a) => LpDestination::SendTo(a.to_string()),
        }
    }
}

/// Sink mutation surface. Transitions are one-shot:
/// `Pending → Settled | Refunded`. Refused exec attempts past terminal
/// states fail with [`crate::error::ContractError::SinkTerminal`].
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SinkStatus {
    Pending,
    Settled,
    Refunded,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, JsonSchema)]
pub struct SinkState {
    pub status: SinkStatus,
    /// Pair created at `Settle` time — `None` until then. Read off
    /// `factory.Pair { asset_infos }` in the `ProvideLiquidity` callback.
    pub pair_addr: Option<Addr>,
    /// LP minted at `ProvideLiquidity` — `None` until `DistributeLp` runs.
    pub lp_minted: Option<Uint128>,
}

pub const ROLE: Item<Role> = Item::new("role");
pub const FACTORY_CONFIG: Item<FactoryConfig> = Item::new("factory_config");
pub const SINK_CONFIG: Item<SinkConfig> = Item::new("sink_config");
pub const SINK_STATE: Item<SinkState> = Item::new("sink_state");
pub const LOCKER_CONFIG: Item<LockerConfig> = Item::new("locker_config");
