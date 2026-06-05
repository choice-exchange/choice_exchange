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
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, JsonSchema)]
pub struct FactoryConfig {
    pub admin: Addr,
    /// Code-id used by `Instantiate2`. Mutable via `UpdateSinkCodeId`.
    /// Single-binary deploys set this == own code-id at instantiate; the
    /// admin can repoint to a freshly-audited build later.
    pub sink_code_id: u64,
    /// Immutable: pinning a factory to one DEX deployment.
    pub choice_factory: Addr,
    /// Immutable cap on per-sink `tip_bps`.
    pub max_tip_bps: u16,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, JsonSchema)]
pub struct SinkConfig {
    pub issuer: Addr,
    pub token_denom: String,
    pub pair_denom: String,
    pub token_decimals: u8,
    pub pair_decimals: u8,
    pub lp_destination: LpDestinationStored,
    pub refund_receiver: Addr,
    pub deadline_seconds: u64,
    /// Wall-clock seconds at instantiate. `Refund`'s permissionless gate
    /// opens at `instantiated_at + deadline_seconds`.
    pub instantiated_at: u64,
    pub tip_bps: u16,
    pub choice_factory: Addr,
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
