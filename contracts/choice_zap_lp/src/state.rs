use cosmwasm_std::{Addr, Empty, Uint128};
use cw_storage_plus::{Item, Map};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use choice::asset::AssetInfo;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, JsonSchema)]
pub struct Config {
    pub owner: Addr,
    /// Fallback recipient for LP+dust. The `ZapBalance` keeper path is locked
    /// to this address; the owner-only `Zap` path falls back to it.
    pub default_recipient: Option<Addr>,
    /// Caller tip on `ZapBalance`, in basis points of the input balance.
    /// Capped at MAX_TIP_BPS in `update_config` / `instantiate`.
    pub tip_bps: u16,
    /// Minimum input-side balance that `ZapBalance` will act on. Below this
    /// the call no-ops (errors). Prevents dust-spam pokes.
    pub min_zap_amount: Uint128,
    /// Royalty route: input asset that `ZapBalance` drains. Set once at
    /// instantiate (or migrate) — immutable from execute. One instance per
    /// `(input, pair)` royalty stream.
    pub input: AssetInfo,
    /// Royalty route: pair contract the input is zapped into. Set once at
    /// instantiate (or migrate) — immutable from execute.
    pub pair: Addr,
}

pub const CONFIG: Item<Config> = Item::new("config");

/// Owner-managed allowlist of keeper addresses authorized to call
/// `ZapBalance`. Owner is implicitly allowed and does not need to be in this
/// map.
pub const KEEPERS: Map<&Addr, Empty> = Map::new("keepers");
