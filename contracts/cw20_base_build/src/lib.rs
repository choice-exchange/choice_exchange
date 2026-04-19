//! Build-only crate: re-exports cw20-base entry points so the
//! workspace-optimizer emits a `cw20_base_build.wasm` integration tests can
//! upload. Not a production contract — don't deploy this on-chain.

use cosmwasm_std::{entry_point, Binary, Deps, DepsMut, Env, MessageInfo, Response, StdResult};
use cw20_base::contract;
use cw20_base::msg::{ExecuteMsg, InstantiateMsg, MigrateMsg, QueryMsg};
use cw20_base::ContractError;

#[entry_point]
pub fn instantiate(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
    msg: InstantiateMsg,
) -> Result<Response, ContractError> {
    contract::instantiate(deps, env, info, msg)
}

#[entry_point]
pub fn execute(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
    msg: ExecuteMsg,
) -> Result<Response, ContractError> {
    contract::execute(deps, env, info, msg)
}

#[entry_point]
pub fn query(deps: Deps, env: Env, msg: QueryMsg) -> StdResult<Binary> {
    contract::query(deps, env, msg)
}

#[entry_point]
pub fn migrate(deps: DepsMut, env: Env, msg: MigrateMsg) -> Result<Response, ContractError> {
    contract::migrate(deps, env, msg)
}
