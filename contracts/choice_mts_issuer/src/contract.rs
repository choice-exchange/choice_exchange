use crate::error::ContractError;
use crate::msg::{
    ClmmPoolAuth, ConfigResponse, ExecuteMsg, InstantiateMsg, LaunchResponse, LaunchesResponse,
    MigrateMsg, QueryMsg,
};
use crate::proto::{
    MsgBurn, MsgChangeAdmin, MsgCreateDenom, MsgCreateTokenPair, ProtoCoin, TokenPair,
};
use crate::state::{Config, LaunchRecord, LaunchStatus, CONFIG, LAUNCHES};

use choice_clmm_common::factory::ExecuteMsg as ClmmFactoryExecuteMsg;
use choice_clmm_common::types::AssetInfo as ClmmAssetInfo;
use choice_pool_seeder::msg::{ExecuteMsg as SeederExecuteMsg, PoolKind};

#[cfg(not(feature = "library"))]
use cosmwasm_std::entry_point;

use cosmwasm_std::{
    coins, to_json_binary, BankMsg, Binary, Coin, CosmosMsg, Deps, DepsMut, Env, MessageInfo,
    Order, Reply, ReplyOn, Response, StdResult, SubMsg, SubMsgResult, Uint128, WasmMsg,
};
use cw2::set_contract_version;
use cw_storage_plus::Bound;
use prost::Message as _;

use injective_cosmwasm::msg::create_mint_tokens_msg;
use injective_cosmwasm::query::InjectiveQueryWrapper;
use injective_cosmwasm::InjectiveMsgWrapper;

const CONTRACT_NAME: &str = "crates.io:choice-mts-issuer";
const CONTRACT_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Reserve room for a `u64` decimal suffix (up to 20 chars) plus the
/// underscore separator inside tokenfactory's 44-char subdenom cap.
pub const MAX_SUBDENOM_PREFIX_LEN: usize = 12;

/// Tokenfactory's hard subdenom length cap. The constructed
/// `{prefix}_{internal_id}[_{salt_suffix}]` must fit within this.
pub const MAX_SUBDENOM_LEN: usize = 44;

/// Cap registration's decimals at the EVM ERC20 norm. MTS pairing inherits
/// this on the EVM side, so anything > 18 risks the auto-deployed
/// `MintBurnBankERC20` failing on construction. Matches the chain-side cap.
pub const MAX_DECIMALS: u32 = 18;

/// Default pagination size for `QueryMsg::Launches`.
const DEFAULT_QUERY_LIMIT: u32 = 30;
/// Hard cap to keep a single query response below the wasm reply size limit.
const MAX_QUERY_LIMIT: u32 = 100;

/// Reply id for the in-line `MsgCreateTokenPair` sub-message. The reply
/// handler decodes the response data and patches `erc20_address` into the
/// matching `LaunchRecord`.
const REPLY_CREATE_TOKEN_PAIR: u64 = 1;

/// 20-zero-byte bech32 burn address. `MsgChangeAdmin` to this address is the
/// canonical tokenfactory-admin "renounce" on Injective (an empty `new_admin`
/// is chain-rejected). See `feedback_inj_tokenfactory_admin_revoke_burn_bech32`
/// and finding C-M2.
const DEAD_TOKENFACTORY_ADMIN: &str = "inj1qqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqe2hm49";

/// Payload carried into the `MsgCreateTokenPair` reply so the handler can
/// locate the launch by its composite `(evm_authority, internal_id)` key
/// (finding C-H1 — `internal_id` alone is no longer globally unique).
#[derive(serde::Serialize, serde::Deserialize)]
struct ReplyPayload {
    evm_authority: String,
    internal_id: u64,
}

#[cfg_attr(not(feature = "library"), entry_point)]
pub fn instantiate(
    deps: DepsMut<InjectiveQueryWrapper>,
    _env: Env,
    _info: MessageInfo,
    msg: InstantiateMsg,
) -> Result<Response<InjectiveMsgWrapper>, ContractError> {
    set_contract_version(deps.storage, CONTRACT_NAME, CONTRACT_VERSION)?;

    validate_subdenom_prefix(&msg.subdenom_prefix)?;
    if msg.decimals > MAX_DECIMALS {
        return Err(ContractError::DecimalsOutOfRange { got: msg.decimals });
    }

    let admin = deps.api.addr_validate(&msg.admin)?;
    let keeper = deps.api.addr_validate(&msg.keeper)?;
    let forwarder = deps.api.addr_validate(&msg.forwarder)?;

    CONFIG.save(
        deps.storage,
        &Config {
            admin: admin.clone(),
            pending_admin: None,
            keeper: keeper.clone(),
            subdenom_prefix: msg.subdenom_prefix.clone(),
            decimals: msg.decimals,
            forwarder: forwarder.clone(),
            refund_deadline_seconds: msg.refund_deadline_seconds,
            paused: false,
        },
    )?;

    Ok(Response::new()
        .add_attribute("action", "instantiate")
        .add_attribute("admin", admin)
        .add_attribute("keeper", keeper)
        .add_attribute("forwarder", forwarder)
        .add_attribute("subdenom_prefix", msg.subdenom_prefix)
        .add_attribute("decimals", msg.decimals.to_string())
        .add_attribute(
            "refund_deadline_seconds",
            msg.refund_deadline_seconds.to_string(),
        ))
}

fn validate_subdenom_prefix(prefix: &str) -> Result<(), ContractError> {
    if prefix.is_empty() || prefix.len() > MAX_SUBDENOM_PREFIX_LEN {
        return Err(ContractError::SubdenomPrefixInvalid {
            got: prefix.to_string(),
            max: MAX_SUBDENOM_PREFIX_LEN,
        });
    }
    // Tokenfactory accepts alphanumeric subdenoms; reject anything else
    // up-front so we don't get a cryptic chain-side error on CreateDenom.
    if !prefix
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_')
    {
        return Err(ContractError::SubdenomPrefixInvalid {
            got: prefix.to_string(),
            max: MAX_SUBDENOM_PREFIX_LEN,
        });
    }
    Ok(())
}

#[cfg_attr(not(feature = "library"), entry_point)]
pub fn execute(
    deps: DepsMut<InjectiveQueryWrapper>,
    env: Env,
    info: MessageInfo,
    msg: ExecuteMsg,
) -> Result<Response<InjectiveMsgWrapper>, ContractError> {
    match msg {
        ExecuteMsg::RegisterLaunch {
            internal_id,
            evm_authority,
            total_supply,
            evm_supply,
            pair_denom,
            seeder_factory,
            seeder_addr,
            create_sink_payload,
            choice_factory,
            salt_suffix,
            clmm_pool_auth,
        } => execute_register_launch(
            deps,
            env,
            info,
            internal_id,
            evm_authority,
            total_supply,
            evm_supply,
            pair_denom,
            seeder_factory,
            seeder_addr,
            create_sink_payload,
            choice_factory,
            salt_suffix,
            clmm_pool_auth,
        ),
        ExecuteMsg::DeliverToSeeder {
            evm_authority,
            internal_id,
            leftover,
        } => execute_deliver_to_seeder(deps, env, info, evm_authority, internal_id, leftover),
        ExecuteMsg::RefundFailedLaunch {
            evm_authority,
            internal_id,
            reason,
        } => execute_refund_failed_launch(deps, env, info, evm_authority, internal_id, reason),
        ExecuteMsg::RenounceDenomAdmin {
            evm_authority,
            internal_id,
        } => execute_renounce_denom_admin(deps, env, info, evm_authority, internal_id),
        ExecuteMsg::UpdateAdmin { new_admin } => execute_update_admin(deps, info, new_admin),
        ExecuteMsg::AcceptAdmin {} => execute_accept_admin(deps, info),
        ExecuteMsg::UpdateKeeper { new_keeper } => execute_update_keeper(deps, info, new_keeper),
        ExecuteMsg::UpdateForwarder { new_forwarder } => {
            execute_update_forwarder(deps, info, new_forwarder)
        }
        ExecuteMsg::SetPaused { paused } => execute_set_paused(deps, info, paused),
        ExecuteMsg::UpdateRefundDeadline {
            new_refund_deadline_seconds,
        } => execute_update_refund_deadline(deps, info, new_refund_deadline_seconds),
        ExecuteMsg::UpdateDecimals { new_decimals } => {
            execute_update_decimals(deps, info, new_decimals)
        }
    }
}

#[allow(clippy::too_many_arguments, clippy::vec_init_then_push)]
fn execute_register_launch(
    deps: DepsMut<InjectiveQueryWrapper>,
    env: Env,
    info: MessageInfo,
    internal_id: u64,
    evm_authority: String,
    total_supply: Uint128,
    evm_supply: Uint128,
    pair_denom: String,
    seeder_factory: String,
    seeder_addr: String,
    create_sink_payload: Binary,
    choice_factory: Option<String>,
    salt_suffix: Option<String>,
    clmm_pool_auth: Option<ClmmPoolAuth>,
) -> Result<Response<InjectiveMsgWrapper>, ContractError> {
    let config = CONFIG.load(deps.storage)?;

    // Circuit breaker: halt NEW launches during an incident. Completion and
    // wind-down of in-flight launches stay open (those handlers don't check
    // `paused`) so a pause never strands a launch mid-graduation.
    if config.paused {
        return Err(ContractError::Paused {});
    }

    // Keeper-gated (finding C-H1). Previously permissionless, which let anyone
    // front-run/squat an `internal_id` for one create-denom fee and brick a
    // launch. The keeper is the only legitimate caller.
    if info.sender != config.keeper {
        return Err(ContractError::NotKeeper {});
    }

    // Validate the authority up front: it is half of the launch's storage key
    // (finding C-H1), so the collision check below needs it.
    let evm_authority = deps.api.addr_validate(&evm_authority)?;

    // Tokenfactory creation fee for the launch denom (`MsgCreateDenom` below).
    // The chain debits this from the issuer's bank balance at dispatch time.
    // Caller must attach the fee in `info.funds` EXACTLY — same denom set,
    // same amounts. Over-pay and extra denoms are rejected so the contract
    // never holds caller-belonging dust. The keeper reads the live chain fee
    // in preflight and retries on governance fee change.
    let create_fee = choice::querier::query_token_factory_denom_create_fee(&deps.querier)?;
    require_exact_create_fee_funds(&info, &create_fee)?;

    // Per-`evm_authority` id namespace: a redeploy that resets its counter to
    // 0 cannot collide with a prior deployment's records.
    if LAUNCHES.has(deps.storage, (&evm_authority, internal_id)) {
        return Err(ContractError::LaunchAlreadyRegistered { id: internal_id });
    }
    if total_supply.is_zero() {
        return Err(ContractError::ZeroTotalSupply {});
    }
    if evm_supply > total_supply {
        return Err(ContractError::EvmSupplyExceedsTotal {
            evm_supply: evm_supply.to_string(),
            total_supply: total_supply.to_string(),
        });
    }
    let mut cw_held = total_supply.checked_sub(evm_supply)?;

    let seeder_factory = deps.api.addr_validate(&seeder_factory)?;
    let seeder_addr = deps.api.addr_validate(&seeder_addr)?;
    let choice_factory_addr = choice_factory
        .as_deref()
        .map(|s| deps.api.addr_validate(s))
        .transpose()?;

    // Reserve 1 wei of `cw_held` for the AddNativeTokenDecimals dust if
    // we're chaining the registration. The dust funds the choice_factory's
    // "non-zero own-balance" verification path — see
    // `choice_factory::contract::execute_add_native_token_decimals`. The
    // consumer dApp's `cw_held` shrinks by 1 wei; negligible at 1e27 scales
    // but the consumer must have left ≥ 1 wei of room.
    if choice_factory_addr.is_some() {
        if cw_held.is_zero() {
            return Err(ContractError::ChoiceFactoryNeedsDust {
                total_supply: total_supply.to_string(),
                evm_supply: evm_supply.to_string(),
            });
        }
        cw_held = cw_held.checked_sub(Uint128::one())?;
    }

    // Layer A: fold the keeper-chosen per-launch salt into the subdenom so the
    // launch denom is unguessable until this tx exists. Validate the suffix and
    // the resulting subdenom length up front — a too-long subdenom would only
    // surface as a cryptic chain-side error on `MsgCreateDenom`.
    //
    // NOTE (C-H1 caveat): the storage key is namespaced by `evm_authority`, but
    // the denom string is `{prefix}_{internal_id}[_{salt}]` and does NOT embed
    // the authority. To run multiple LaunchpadCore deployments under ONE issuer
    // instance without an on-chain denom collision on a reused `internal_id`,
    // the operator must give each deployment a distinct `subdenom_prefix` (own
    // issuer instance) OR a per-deployment `salt_suffix`. The keeper-gate +
    // per-authority storage key already prevent any record hijack/overwrite.
    let subdenom = match &salt_suffix {
        Some(salt) => {
            if salt.is_empty() || !salt.chars().all(|c| c.is_ascii_alphanumeric()) {
                return Err(ContractError::SaltSuffixInvalid { got: salt.clone() });
            }
            format!("{}_{}_{}", config.subdenom_prefix, internal_id, salt)
        }
        None => format!("{}_{}", config.subdenom_prefix, internal_id),
    };
    if subdenom.len() > MAX_SUBDENOM_LEN {
        return Err(ContractError::SubdenomTooLong {
            subdenom: subdenom.clone(),
            len: subdenom.len(),
            max: MAX_SUBDENOM_LEN,
        });
    }
    let denom = format!("factory/{}/{}", env.contract.address, subdenom);

    // The pair asset must differ from the freshly-minted launch denom. A
    // self-paired pool is degenerate and would fail (or mis-seed) at the
    // seeder's Settle; reject early with a clear error instead of letting a
    // keeper typo surface as a cryptic downstream revert.
    if pair_denom == denom {
        return Err(ContractError::PairDenomEqualsLaunchDenom {
            denom: denom.clone(),
        });
    }

    // Decode the opaque seeder payload so the issuer can guarantee, on-chain:
    //   (a) the sink seeds EXACTLY the launch denom + pair we create/reserve
    //       here (a misbehaving keeper can't point the sink at a different
    //       denom/pair than the one this record and any reservation cover),
    //   (b) a CLMM launch ALWAYS reserves its pool slot — `clmm_pool_auth` is
    //       mandatory and its `fee`/`clmm_factory` must match the sink's, else
    //       the slot is squattable between RegisterLaunch and Settle, and
    //   (c) an XYK launch carries no stray CLMM reservation.
    // Importing the real seeder `msg` types makes a future field rename a
    // compile error here rather than a silent enforcement bypass.
    let sink_init = match cosmwasm_std::from_json::<SeederExecuteMsg>(&create_sink_payload)
        .map_err(|e| ContractError::SinkPayloadDecode(e.to_string()))?
    {
        SeederExecuteMsg::CreateSink { sink_init, .. } => sink_init,
        _ => {
            return Err(ContractError::SinkPayloadDecode(
                "expected a CreateSink message".to_string(),
            ))
        }
    };
    if sink_init.token_denom != denom {
        return Err(ContractError::SinkPayloadMismatch {
            field: "token_denom".to_string(),
            got: sink_init.token_denom,
            expected: denom.clone(),
        });
    }
    if sink_init.pair_denom != pair_denom {
        return Err(ContractError::SinkPayloadMismatch {
            field: "pair_denom".to_string(),
            got: sink_init.pair_denom,
            expected: pair_denom.clone(),
        });
    }
    match &sink_init.pool_kind {
        PoolKind::Clmm {
            clmm_factory: sink_clmm_factory,
            fee_tier,
            ..
        } => {
            let auth = clmm_pool_auth
                .as_ref()
                .ok_or(ContractError::ClmmAuthRequired {})?;
            if auth.fee != *fee_tier {
                return Err(ContractError::ClmmAuthFeeMismatch {
                    auth_fee: auth.fee,
                    sink_fee: *fee_tier,
                });
            }
            if &auth.clmm_factory != sink_clmm_factory {
                return Err(ContractError::ClmmAuthFactoryMismatch {
                    auth_factory: auth.clmm_factory.clone(),
                    sink_factory: sink_clmm_factory.clone(),
                });
            }
        }
        PoolKind::Xyk { .. } => {
            if clmm_pool_auth.is_some() {
                return Err(ContractError::ClmmAuthUnexpected {});
            }
        }
    }

    // Persist the record up front. `erc20_address` lands in the reply
    // handler; the rest of the record is final at this point and the tx
    // either fully succeeds (status stays Registered) or fully reverts.
    let record = LaunchRecord {
        internal_id,
        status: LaunchStatus::Registered,
        denom: denom.clone(),
        evm_authority: evm_authority.clone(),
        total_supply,
        evm_supply,
        cw_held,
        pair_denom: pair_denom.clone(),
        seeder_factory: seeder_factory.clone(),
        seeder_addr: seeder_addr.clone(),
        registered_at: env.block.time.seconds(),
        erc20_address: None,
        choice_factory: choice_factory_addr.clone(),
        admin_renounced: false,
    };
    LAUNCHES.save(deps.storage, (&evm_authority, internal_id), &record)?;

    // Build the message chain in dispatch order — chain semantics require
    // CreateDenom and Mint to land before the CreateTokenPair SubMsg can
    // reference the denom. The previous shape (`add_submessages` then
    // `add_messages`) put CreateTokenPair at index 0 of the response,
    // which would fail on real chain dispatch; this builder keeps the
    // CreateTokenPair SubMsg in its correct ordinal position (after Mint).
    let mut submsgs: Vec<SubMsg<InjectiveMsgWrapper>> = Vec::new();

    // 1. CreateDenom with allow_admin_burn=true. Stargate (the
    //    injective_cosmwasm 0.3.4-1 helper doesn't expose the new field).
    submsgs.push(SubMsg::new(CosmosMsg::from(MsgCreateDenom {
        sender: env.contract.address.to_string(),
        subdenom: subdenom.clone(),
        name: subdenom.clone(),
        symbol: subdenom.to_uppercase(),
        decimals: config.decimals,
        allow_admin_burn: true,
    })));

    // 2. (removed) SetTokenMetadata.
    //
    // injectived v1.20+ `MsgCreateDenom` finalises decimals at create-time,
    // and a follow-up `MsgSetDenomMetadata` with the same decimals errors
    // out with "cannot update denom metadata decimals". Empirically
    // observed on testnet 2026-05-26 (tx 52D5873E…). The decimals are
    // already on-chain from step 1; name/symbol on the chain-side metadata
    // can be set later via a separate, decimals-free path if needed
    // (cosmetic only — choice_factory uses its own NATIVE_TOKEN_DECIMALS
    // map, registered via step 5 below).

    // 3. Mint full total_supply to self. cw_held stays here (minus 1 wei
    //    dust if step 5 fires); evm_supply is bank-sent out at step 6.
    submsgs.push(SubMsg::new(create_mint_tokens_msg(
        env.contract.address.clone(),
        Coin {
            denom: denom.clone(),
            amount: total_supply,
        },
        env.contract.address.to_string(),
    )));

    // 4. CreateTokenPair as SubMsg with reply_on_success: we want the
    //    auto-deployed ERC20 address captured into LaunchRecord. Errors
    //    abort the whole tx (state changes above roll back).
    submsgs.push(SubMsg {
        id: REPLY_CREATE_TOKEN_PAIR,
        payload: to_json_binary(&ReplyPayload {
            evm_authority: evm_authority.to_string(),
            internal_id,
        })?,
        msg: MsgCreateTokenPair {
            sender: env.contract.address.to_string(),
            token_pair: Some(TokenPair {
                bank_denom: denom.clone(),
                erc20_address: String::new(),
            }),
        }
        .into(),
        gas_limit: None,
        reply_on: ReplyOn::Success,
    });

    // 5. (Optional) AddNativeTokenDecimals on the consumer-chosen
    //    choice_factory. Funded with 1 wei of `denom` so the factory's
    //    `query_balance(this, denom)` verification passes. The dust
    //    permanently lives in the factory's bank balance (rescuable via
    //    its admin's `WithdrawNative`). The issuer is the only entity
    //    authorized to register a `factory/<this>/...` denom; see
    //    `feedback_inj_bank_precompile_20byte` for why this hop can't be
    //    deferred to a separate keeper tx.
    if let Some(addr) = &choice_factory_addr {
        submsgs.push(SubMsg::new(CosmosMsg::Wasm(WasmMsg::Execute {
            contract_addr: addr.to_string(),
            msg: to_json_binary(&choice::factory::ExecuteMsg::AddNativeTokenDecimals {
                denom: denom.clone(),
                decimals: config.decimals as u8,
            })?,
            funds: coins(1u128, &denom),
        })));
    }

    // 5b. (Optional) Layer B anti-squat gate. Reserve the CLMM pool slot
    //     `(launch_denom, pair_denom, fee)` for the sink that will create it at
    //     Settle. The issuer owns the `factory/{this}/...` namespace, so the
    //     factory authorizes this reservation; afterwards no squatter can
    //     occupy the slot during the curve's lifetime. `ttl_seconds = 0` =
    //     no expiry — safe because the launch denom is unique. Auth doesn't
    //     require the denom to exist yet (pure string parse on the factory
    //     side), but we emit it after CreateDenom anyway for clarity.
    if let Some(auth) = &clmm_pool_auth {
        let clmm_factory = deps.api.addr_validate(&auth.clmm_factory)?;
        submsgs.push(SubMsg::new(CosmosMsg::Wasm(WasmMsg::Execute {
            contract_addr: clmm_factory.to_string(),
            msg: to_json_binary(&ClmmFactoryExecuteMsg::AuthorizeCreation {
                token_a: ClmmAssetInfo::NativeToken {
                    denom: denom.clone(),
                },
                token_b: ClmmAssetInfo::NativeToken {
                    denom: pair_denom.clone(),
                },
                fee: auth.fee,
                creator: seeder_addr.to_string(),
                ttl_seconds: auth.ttl_seconds,
            })?,
            funds: vec![],
        })));
    }

    // 6. Forward the opaque CreateSink payload to the consumer dApp's
    //    chosen seeder factory. The issuer carries no funds on this hop —
    //    the sink fills its bank balances from Leg B (this contract →
    //    seeder_addr, in step 7) and Leg C (forwarder → seeder_addr,
    //    EVM-side).
    submsgs.push(SubMsg::new(CosmosMsg::Wasm(WasmMsg::Execute {
        contract_addr: seeder_factory.to_string(),
        msg: create_sink_payload,
        funds: vec![],
    })));

    // 7. Ship evm_supply to the EVM authority's bech32. After this the
    //    authority's bank balance funds the curve / fair-launch trading.
    if !evm_supply.is_zero() {
        submsgs.push(SubMsg::new(CosmosMsg::Bank(BankMsg::Send {
            to_address: evm_authority.to_string(),
            amount: coins(evm_supply.u128(), &denom),
        })));
    }

    Ok(Response::new()
        .add_submessages(submsgs)
        .add_attribute("action", "register_launch")
        .add_attribute("internal_id", internal_id.to_string())
        .add_attribute("denom", denom)
        .add_attribute("evm_authority", evm_authority)
        .add_attribute("total_supply", total_supply)
        .add_attribute("evm_supply", evm_supply)
        .add_attribute("cw_held", cw_held)
        .add_attribute("seeder_factory", seeder_factory)
        .add_attribute("seeder_addr", seeder_addr)
        .add_attribute("pair_denom", pair_denom)
        .add_attribute(
            "choice_factory",
            choice_factory_addr
                .map(|a| a.to_string())
                .unwrap_or_else(|| "none".to_string()),
        ))
}

fn execute_deliver_to_seeder(
    deps: DepsMut<InjectiveQueryWrapper>,
    env: Env,
    info: MessageInfo,
    evm_authority: String,
    internal_id: u64,
    leftover: Uint128,
) -> Result<Response<InjectiveMsgWrapper>, ContractError> {
    let config = CONFIG.load(deps.storage)?;
    if info.sender != config.keeper {
        return Err(ContractError::NotKeeper {});
    }

    let evm_authority = deps.api.addr_validate(&evm_authority)?;
    let mut record = LAUNCHES
        .may_load(deps.storage, (&evm_authority, internal_id))?
        .ok_or(ContractError::LaunchNotFound { id: internal_id })?;

    if record.status != LaunchStatus::Registered {
        return Err(ContractError::InvalidLaunchStatus {
            id: internal_id,
            actual: format!("{:?}", record.status),
            expected: "Registered".to_string(),
        });
    }
    if leftover > record.evm_supply {
        return Err(ContractError::LeftoverExceedsEvmSupply {
            leftover: leftover.to_string(),
            evm_supply: record.evm_supply.to_string(),
        });
    }

    // Finding C-M3: refuse to bank-send `cw_held` to a `seeder_addr` that
    // holds no contract code. The sink is instantiated in the RegisterLaunch
    // tx (the forwarded `CreateSink`), so by the time the keeper cranks
    // DeliverToSeeder it must exist; a missing contract means the off-chain
    // instantiate2 address was wrong and the funds would land at a ghost
    // address unrecoverably. `query_wasm_contract_info` errors when there is
    // no contract at the address.
    if !record.cw_held.is_zero()
        && deps
            .querier
            .query_wasm_contract_info(record.seeder_addr.as_str())
            .is_err()
    {
        return Err(ContractError::SeederAddrNotAContract {
            addr: record.seeder_addr.to_string(),
        });
    }

    let mut messages: Vec<CosmosMsg<InjectiveMsgWrapper>> = Vec::new();

    // Leg A — burn the unsold EVM-side supply from the authority's bech32.
    // Burn what the authority ACTUALLY holds (capped at `evm_supply`) rather
    // than trusting the keeper-relayed `leftover`. This removes the trust
    // assumption: a keeper that under-reports `leftover` can no longer strand
    // unsold supply, and the `evm_supply` cap stops a third party from tricking
    // us into burning unrelated tokens donated to that address. `leftover` is
    // retained only as a keeper-input sanity bound (checked above) and an audit
    // attribute. Requires `allow_admin_burn=true`, which RegisterLaunch
    // guarantees.
    let evm_held = deps
        .querier
        .query_balance(record.evm_authority.as_str(), &record.denom)?
        .amount;
    let to_burn = evm_held.min(record.evm_supply);
    if !to_burn.is_zero() {
        messages.push(
            MsgBurn {
                sender: env.contract.address.to_string(),
                amount: Some(ProtoCoin {
                    denom: record.denom.clone(),
                    amount: to_burn.to_string(),
                }),
                burn_from_address: record.evm_authority.to_string(),
            }
            .into(),
        );
    }

    // Leg B — deliver cw_held to the seeder sink. Bank-send is fine because
    // both addresses are on the same VM.
    if !record.cw_held.is_zero() {
        messages.push(CosmosMsg::Bank(BankMsg::Send {
            to_address: record.seeder_addr.to_string(),
            amount: coins(record.cw_held.u128(), &record.denom),
        }));
    }

    record.status = LaunchStatus::Delivered;
    LAUNCHES.save(deps.storage, (&evm_authority, internal_id), &record)?;

    Ok(Response::new()
        .add_messages(messages)
        .add_attribute("action", "deliver_to_seeder")
        .add_attribute("internal_id", internal_id.to_string())
        .add_attribute("denom", record.denom)
        .add_attribute("leftover_reported", leftover)
        .add_attribute("leftover_burned", to_burn)
        .add_attribute("cw_held_delivered", record.cw_held)
        .add_attribute("seeder_addr", record.seeder_addr))
}

fn execute_refund_failed_launch(
    deps: DepsMut<InjectiveQueryWrapper>,
    env: Env,
    info: MessageInfo,
    evm_authority: String,
    internal_id: u64,
    reason: String,
) -> Result<Response<InjectiveMsgWrapper>, ContractError> {
    let config = CONFIG.load(deps.storage)?;

    let evm_authority = deps.api.addr_validate(&evm_authority)?;
    let mut record = LAUNCHES
        .may_load(deps.storage, (&evm_authority, internal_id))?
        .ok_or(ContractError::LaunchNotFound { id: internal_id })?;

    if record.status != LaunchStatus::Registered {
        return Err(ContractError::InvalidLaunchStatus {
            id: internal_id,
            actual: format!("{:?}", record.status),
            expected: "Registered".to_string(),
        });
    }

    // The keeper can refund any time. After the deadline the `admin` (the
    // consumer dApp's timelock/multisig) may also refund, covering a keeper
    // outage. It is deliberately NOT fully permissionless: a wide-open refund
    // would let anyone burn `cw_held` and terminally `Refunded` a slow-but-
    // valid launch out from under graduation (the launch stays `Registered`
    // for its whole curve lifetime). Both keeper and admin are trusted
    // parties, so the only thing the post-deadline path grants is liveness,
    // never the power to grief a healthy launch.
    if info.sender != config.keeper {
        let deadline = record
            .registered_at
            .saturating_add(config.refund_deadline_seconds);
        let now = env.block.time.seconds();
        if now < deadline {
            return Err(ContractError::RefundDeadlineNotReached {
                remaining_seconds: deadline - now,
            });
        }
        if info.sender != config.admin {
            return Err(ContractError::Unauthorized {});
        }
    }

    let mut messages: Vec<CosmosMsg<InjectiveMsgWrapper>> = Vec::new();

    // Burn cw_held from self (the contract's bank balance still holds it
    // because Registered → Refunded skips DeliverToSeeder). Uses the simple
    // self-burn path — no `burn_from_address` needed, the injective_cosmwasm
    // helper is fine for this side.
    if !record.cw_held.is_zero() {
        messages.push(injective_cosmwasm::msg::create_burn_tokens_msg(
            env.contract.address.clone(),
            Coin {
                denom: record.denom.clone(),
                amount: record.cw_held,
            },
        ));
    }

    // Also burn the EVM-side supply. RegisterLaunch bank-sent `evm_supply` to
    // `evm_authority`; on a failed launch that balance is dangling, worthless
    // launch-denom supply that ONLY the issuer (still the tokenfactory admin
    // here — RenounceDenomAdmin requires `Delivered`) can clean up. Burn what
    // the authority actually holds, capped at `evm_supply` so a third party
    // can't trick us into burning unrelated tokens sent to that address.
    // Requires `allow_admin_burn=true`, which RegisterLaunch guarantees.
    let evm_held = deps
        .querier
        .query_balance(record.evm_authority.as_str(), &record.denom)?
        .amount;
    let evm_burn = evm_held.min(record.evm_supply);
    if !evm_burn.is_zero() {
        messages.push(
            MsgBurn {
                sender: env.contract.address.to_string(),
                amount: Some(ProtoCoin {
                    denom: record.denom.clone(),
                    amount: evm_burn.to_string(),
                }),
                burn_from_address: record.evm_authority.to_string(),
            }
            .into(),
        );
    }

    record.status = LaunchStatus::Refunded;
    LAUNCHES.save(deps.storage, (&evm_authority, internal_id), &record)?;

    Ok(Response::new()
        .add_messages(messages)
        .add_attribute("action", "refund_failed_launch")
        .add_attribute("internal_id", internal_id.to_string())
        .add_attribute("denom", record.denom)
        .add_attribute("cw_held_burned", record.cw_held)
        .add_attribute("evm_supply_burned", evm_burn)
        .add_attribute("reason", reason))
}

/// Finding C-M2: relinquish this contract's tokenfactory admin over a launch
/// denom once the launch is `Delivered`, by rotating the admin to the
/// burn-address convention. After this the issuer can no longer `MsgMint` or
/// admin-`MsgBurn`-from for the denom. Callable by `keeper` or `admin`.
fn execute_renounce_denom_admin(
    deps: DepsMut<InjectiveQueryWrapper>,
    env: Env,
    info: MessageInfo,
    evm_authority: String,
    internal_id: u64,
) -> Result<Response<InjectiveMsgWrapper>, ContractError> {
    let config = CONFIG.load(deps.storage)?;
    if info.sender != config.keeper && info.sender != config.admin {
        return Err(ContractError::Unauthorized {});
    }

    let evm_authority = deps.api.addr_validate(&evm_authority)?;
    let mut record = LAUNCHES
        .may_load(deps.storage, (&evm_authority, internal_id))?
        .ok_or(ContractError::LaunchNotFound { id: internal_id })?;

    // Only after delivery: while a launch is still `Registered`, the issuer
    // may still need admin-burn (DeliverToSeeder Leg A) or refund-burn.
    if record.status != LaunchStatus::Delivered {
        return Err(ContractError::InvalidLaunchStatus {
            id: internal_id,
            actual: format!("{:?}", record.status),
            expected: "Delivered".to_string(),
        });
    }
    if record.admin_renounced {
        return Err(ContractError::DenomAdminAlreadyRenounced { id: internal_id });
    }

    record.admin_renounced = true;
    LAUNCHES.save(deps.storage, (&evm_authority, internal_id), &record)?;

    let msg: CosmosMsg<InjectiveMsgWrapper> = MsgChangeAdmin {
        sender: env.contract.address.to_string(),
        denom: record.denom.clone(),
        new_admin: DEAD_TOKENFACTORY_ADMIN.to_string(),
    }
    .into();

    Ok(Response::new()
        .add_message(msg)
        .add_attribute("action", "renounce_denom_admin")
        .add_attribute("evm_authority", evm_authority)
        .add_attribute("internal_id", internal_id.to_string())
        .add_attribute("denom", record.denom)
        .add_attribute("new_admin", DEAD_TOKENFACTORY_ADMIN))
}

/// Step 1 of the two-step admin rotation: park `new_admin` as pending. The
/// live `admin` is untouched until the pending key accepts.
fn execute_update_admin(
    deps: DepsMut<InjectiveQueryWrapper>,
    info: MessageInfo,
    new_admin: String,
) -> Result<Response<InjectiveMsgWrapper>, ContractError> {
    let mut config = CONFIG.load(deps.storage)?;
    if info.sender != config.admin {
        return Err(ContractError::Unauthorized {});
    }
    let new_admin = deps.api.addr_validate(&new_admin)?;
    config.pending_admin = Some(new_admin.clone());
    CONFIG.save(deps.storage, &config)?;
    Ok(Response::new()
        .add_attribute("action", "update_admin")
        .add_attribute("pending_admin", new_admin))
}

/// Step 2 of the rotation: the pending admin claims the role.
fn execute_accept_admin(
    deps: DepsMut<InjectiveQueryWrapper>,
    info: MessageInfo,
) -> Result<Response<InjectiveMsgWrapper>, ContractError> {
    let mut config = CONFIG.load(deps.storage)?;
    let pending = config
        .pending_admin
        .clone()
        .ok_or(ContractError::NoPendingAdmin {})?;
    if info.sender != pending {
        return Err(ContractError::Unauthorized {});
    }
    config.admin = pending.clone();
    config.pending_admin = None;
    CONFIG.save(deps.storage, &config)?;
    Ok(Response::new()
        .add_attribute("action", "accept_admin")
        .add_attribute("new_admin", pending))
}

fn execute_set_paused(
    deps: DepsMut<InjectiveQueryWrapper>,
    info: MessageInfo,
    paused: bool,
) -> Result<Response<InjectiveMsgWrapper>, ContractError> {
    let mut config = CONFIG.load(deps.storage)?;
    if info.sender != config.admin {
        return Err(ContractError::Unauthorized {});
    }
    config.paused = paused;
    CONFIG.save(deps.storage, &config)?;
    Ok(Response::new()
        .add_attribute("action", "set_paused")
        .add_attribute("paused", paused.to_string()))
}

fn execute_update_refund_deadline(
    deps: DepsMut<InjectiveQueryWrapper>,
    info: MessageInfo,
    new_refund_deadline_seconds: u64,
) -> Result<Response<InjectiveMsgWrapper>, ContractError> {
    let mut config = CONFIG.load(deps.storage)?;
    if info.sender != config.admin {
        return Err(ContractError::Unauthorized {});
    }
    config.refund_deadline_seconds = new_refund_deadline_seconds;
    CONFIG.save(deps.storage, &config)?;
    Ok(Response::new()
        .add_attribute("action", "update_refund_deadline")
        .add_attribute(
            "refund_deadline_seconds",
            new_refund_deadline_seconds.to_string(),
        ))
}

/// Retune the default tokenfactory `decimals` for FUTURE launches. Bounded by
/// `MAX_DECIMALS` (the EVM ERC20 norm) — already-created denoms are unaffected.
fn execute_update_decimals(
    deps: DepsMut<InjectiveQueryWrapper>,
    info: MessageInfo,
    new_decimals: u32,
) -> Result<Response<InjectiveMsgWrapper>, ContractError> {
    let mut config = CONFIG.load(deps.storage)?;
    if info.sender != config.admin {
        return Err(ContractError::Unauthorized {});
    }
    if new_decimals > MAX_DECIMALS {
        return Err(ContractError::DecimalsOutOfRange { got: new_decimals });
    }
    config.decimals = new_decimals;
    CONFIG.save(deps.storage, &config)?;
    Ok(Response::new()
        .add_attribute("action", "update_decimals")
        .add_attribute("decimals", new_decimals.to_string()))
}

fn execute_update_keeper(
    deps: DepsMut<InjectiveQueryWrapper>,
    info: MessageInfo,
    new_keeper: String,
) -> Result<Response<InjectiveMsgWrapper>, ContractError> {
    let mut config = CONFIG.load(deps.storage)?;
    if info.sender != config.admin {
        return Err(ContractError::Unauthorized {});
    }
    let new_keeper = deps.api.addr_validate(&new_keeper)?;
    config.keeper = new_keeper.clone();
    CONFIG.save(deps.storage, &config)?;
    Ok(Response::new()
        .add_attribute("action", "update_keeper")
        .add_attribute("new_keeper", new_keeper))
}

fn execute_update_forwarder(
    deps: DepsMut<InjectiveQueryWrapper>,
    info: MessageInfo,
    new_forwarder: String,
) -> Result<Response<InjectiveMsgWrapper>, ContractError> {
    let mut config = CONFIG.load(deps.storage)?;
    if info.sender != config.admin {
        return Err(ContractError::Unauthorized {});
    }
    let new_forwarder = deps.api.addr_validate(&new_forwarder)?;
    config.forwarder = new_forwarder.clone();
    CONFIG.save(deps.storage, &config)?;
    Ok(Response::new()
        .add_attribute("action", "update_forwarder")
        .add_attribute("new_forwarder", new_forwarder))
}

#[cfg_attr(not(feature = "library"), entry_point)]
pub fn reply(
    deps: DepsMut<InjectiveQueryWrapper>,
    _env: Env,
    msg: Reply,
) -> Result<Response<InjectiveMsgWrapper>, ContractError> {
    match msg.id {
        REPLY_CREATE_TOKEN_PAIR => handle_create_token_pair_reply(deps, msg),
        other => Err(ContractError::UnknownReplyId { id: other }),
    }
}

fn handle_create_token_pair_reply(
    deps: DepsMut<InjectiveQueryWrapper>,
    msg: Reply,
) -> Result<Response<InjectiveMsgWrapper>, ContractError> {
    let ReplyPayload {
        evm_authority,
        internal_id,
    } = decode_reply_payload(&msg.payload)?;
    let evm_authority = deps.api.addr_validate(&evm_authority)?;

    let response = match msg.result {
        SubMsgResult::Ok(r) => r,
        // ReplyOn::Success means we shouldn't see an error here. Defensive.
        SubMsgResult::Err(e) => {
            return Err(ContractError::CreateTokenPairReplyDecode(format!(
                "submsg returned error for launch {}: {}",
                internal_id, e
            )));
        }
    };

    #[allow(deprecated)]
    let data = response
        .data
        .ok_or(ContractError::CreateTokenPairReplyMissingData { id: internal_id })?;

    let parsed = crate::proto::MsgCreateTokenPairResponse::decode(data.as_slice())
        .map_err(|e| ContractError::CreateTokenPairReplyDecode(e.to_string()))?;

    // Require a real ERC20 address. An absent `token_pair` or empty
    // `erc20_address` would otherwise persist `Some("")`, silently violating
    // the LaunchRecord invariant (`erc20_address` is always a populated 0x hex
    // once `Registered`) and misleading EVM-side tooling that reads this field.
    // Reject so the whole RegisterLaunch tx reverts rather than half-recording.
    let erc20 = parsed
        .token_pair
        .map(|tp| tp.erc20_address)
        .filter(|s| !s.is_empty())
        .ok_or(ContractError::CreateTokenPairReplyMissingData { id: internal_id })?;

    LAUNCHES.update(
        deps.storage,
        (&evm_authority, internal_id),
        |maybe| -> Result<LaunchRecord, ContractError> {
            let mut rec = maybe.ok_or(ContractError::LaunchNotFound { id: internal_id })?;
            rec.erc20_address = Some(erc20.clone());
            Ok(rec)
        },
    )?;

    Ok(Response::new()
        .add_attribute("action", "reply_create_token_pair")
        .add_attribute("internal_id", internal_id.to_string())
        .add_attribute("erc20_address", erc20))
}

/// Require `info.funds` to be exactly the chain's tokenfactory create fee —
/// same denom set, same per-denom amounts. Over-pay and extra denoms are
/// rejected (rather than refunded) so the contract never accumulates
/// caller-belonging dust and the message chain stays free of late refund
/// hops. The keeper reads the live chain fee in preflight and retries on
/// governance fee change.
pub(crate) fn require_exact_create_fee_funds(
    info: &MessageInfo,
    create_fee: &[Coin],
) -> Result<(), ContractError> {
    for fee in create_fee {
        let supplied = info
            .funds
            .iter()
            .find(|c| c.denom == fee.denom)
            .map(|c| c.amount)
            .unwrap_or_default();
        if supplied < fee.amount {
            return Err(ContractError::InsufficientCreateFee {
                denom: fee.denom.clone(),
                required: fee.amount.to_string(),
                supplied: supplied.to_string(),
            });
        }
        if supplied > fee.amount {
            return Err(ContractError::CreateFeeOverpaid {
                denom: fee.denom.clone(),
                required: fee.amount.to_string(),
                supplied: supplied.to_string(),
            });
        }
    }
    for c in info.funds.iter() {
        if !create_fee.iter().any(|f| f.denom == c.denom) {
            return Err(ContractError::UnexpectedFundsDenom {
                denom: c.denom.clone(),
            });
        }
    }
    Ok(())
}

fn decode_reply_payload(payload: &Binary) -> Result<ReplyPayload, ContractError> {
    cosmwasm_std::from_json(payload)
        .map_err(|e| ContractError::CreateTokenPairReplyDecode(e.to_string()))
}

#[cfg_attr(not(feature = "library"), entry_point)]
pub fn query(deps: Deps<InjectiveQueryWrapper>, _env: Env, msg: QueryMsg) -> StdResult<Binary> {
    match msg {
        QueryMsg::Config {} => to_json_binary(&query_config(deps)?),
        QueryMsg::Launch {
            evm_authority,
            internal_id,
        } => to_json_binary(&query_launch(deps, evm_authority, internal_id)?),
        QueryMsg::Launches {
            evm_authority,
            start_after,
            limit,
        } => to_json_binary(&query_launches(deps, evm_authority, start_after, limit)?),
    }
}

fn query_config(deps: Deps<InjectiveQueryWrapper>) -> StdResult<ConfigResponse> {
    let c = CONFIG.load(deps.storage)?;
    Ok(ConfigResponse {
        admin: c.admin.into_string(),
        pending_admin: c.pending_admin.map(|a| a.into_string()),
        keeper: c.keeper.into_string(),
        subdenom_prefix: c.subdenom_prefix,
        decimals: c.decimals,
        forwarder: c.forwarder.into_string(),
        refund_deadline_seconds: c.refund_deadline_seconds,
        paused: c.paused,
    })
}

fn query_launch(
    deps: Deps<InjectiveQueryWrapper>,
    evm_authority: String,
    internal_id: u64,
) -> StdResult<LaunchResponse> {
    let evm_authority = deps.api.addr_validate(&evm_authority)?;
    let record = LAUNCHES
        .may_load(deps.storage, (&evm_authority, internal_id))?
        .ok_or_else(|| {
            cosmwasm_std::StdError::not_found(format!(
                "launch {} for authority {}",
                internal_id, evm_authority
            ))
        })?;
    Ok(record.into())
}

fn query_launches(
    deps: Deps<InjectiveQueryWrapper>,
    evm_authority: String,
    start_after: Option<u64>,
    limit: Option<u32>,
) -> StdResult<LaunchesResponse> {
    let evm_authority = deps.api.addr_validate(&evm_authority)?;
    let limit = limit.unwrap_or(DEFAULT_QUERY_LIMIT).min(MAX_QUERY_LIMIT) as usize;
    let min = start_after.map(Bound::exclusive);
    // Paginate within one authority's `internal_id` namespace.
    let launches: Vec<LaunchResponse> = LAUNCHES
        .prefix(&evm_authority)
        .range(deps.storage, min, None, Order::Ascending)
        .take(limit)
        .map(|item| item.map(|(_, r)| r.into()))
        .collect::<StdResult<_>>()?;
    Ok(LaunchesResponse { launches })
}

#[cfg_attr(not(feature = "library"), entry_point)]
pub fn migrate(
    deps: DepsMut<InjectiveQueryWrapper>,
    _env: Env,
    msg: MigrateMsg,
) -> Result<Response<InjectiveMsgWrapper>, ContractError> {
    let current = cw2::get_contract_version(deps.storage)?;
    if current.contract != CONTRACT_NAME {
        return Err(ContractError::MigrationWrongContract {
            found: current.contract,
            expected: CONTRACT_NAME.to_string(),
        });
    }
    match msg {
        MigrateMsg::FromV1 {} => {
            if !current.version.starts_with("1.") {
                return Err(ContractError::InvalidMigration {
                    from: current.version,
                    requested: "from_v1".to_string(),
                });
            }
            // No v1 → v2 schema delta yet. The variant exists so the path
            // is wired before consumer dApps integrate; once a real v1 ships
            // and a v2 needs a state delta, fill in the conversion here.
            set_contract_version(deps.storage, CONTRACT_NAME, CONTRACT_VERSION)?;
            Ok(Response::new()
                .add_attribute("action", "migrate")
                .add_attribute("variant", "from_v1")
                .add_attribute("from_version", current.version)
                .add_attribute("to_version", CONTRACT_VERSION))
        }
        MigrateMsg::Patch {} => {
            if !current.version.starts_with("1.") {
                return Err(ContractError::InvalidMigration {
                    from: current.version,
                    requested: "patch".to_string(),
                });
            }
            set_contract_version(deps.storage, CONTRACT_NAME, CONTRACT_VERSION)?;
            Ok(Response::new()
                .add_attribute("action", "migrate")
                .add_attribute("variant", "patch")
                .add_attribute("from_version", current.version)
                .add_attribute("to_version", CONTRACT_VERSION))
        }
    }
}
