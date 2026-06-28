use crate::error::ContractError;
use crate::msg::{
    ClmmPoolAuth, ConfigResponse, ExecuteMsg, InstantiateMsg, LaunchResponse, LaunchesResponse,
    MigrateMsg, QueryMsg,
};
use crate::proto::{
    MsgBurn, MsgChangeAdmin, MsgCreateDenom, MsgCreateTokenPair, ProtoCoin, TokenPair,
};
use crate::state::{Config, LaunchRecord, LaunchStatus, CONFIG, DENOM_INDEX, LAUNCHES};

use choice_clmm_common::factory::ExecuteMsg as ClmmFactoryExecuteMsg;
use choice_clmm_common::types::AssetInfo as ClmmAssetInfo;
use choice_pool_seeder::msg::{
    ExecuteMsg as SeederExecuteMsg, FactoryConfigResponse as SeederFactoryConfig, PoolKind,
    QueryMsg as SeederQueryMsg, SinkConfigResponse as SeederSinkConfig,
};

#[cfg(not(feature = "library"))]
use cosmwasm_std::entry_point;

use cosmwasm_std::{
    coins, instantiate2_address, to_json_binary, Addr, BankMsg, Binary, CanonicalAddr, Coin,
    CosmosMsg, Deps, DepsMut, Env, MessageInfo, Order, Reply, ReplyOn, Response, StdResult, SubMsg,
    SubMsgResult, Uint128, WasmMsg,
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

/// Minimum length of the mandatory Layer A `salt_suffix` (anti-squat entropy,
/// fix for M-2). 8 ASCII-alphanumeric chars ≈ 47 bits of entropy — far beyond
/// what an attacker could pre-compute to squat a launch's sink/locker address
/// before its `RegisterLaunch` tx is even in the mempool. Folded into the
/// subdenom AND the sink/locker `Instantiate2` salts. Bounded above only by the
/// 44-char subdenom cap (`MAX_SUBDENOM_LEN`), checked after concatenation.
pub const MIN_SALT_SUFFIX_LEN: usize = 8;

/// Cap registration's decimals at the EVM ERC20 norm. MTS pairing inherits
/// this on the EVM side, so anything > 18 risks the auto-deployed
/// `MintBurnBankERC20` failing on construction. Matches the chain-side cap.
pub const MAX_DECIMALS: u32 = 18;

/// Floor on `refund_deadline_seconds` (P2-C). The admin-side `RefundFailedLaunch`
/// path only opens this many seconds past a launch's `registered_at`; allowing
/// `0` (or a near-zero value) would let a timelocked-but-compromised admin
/// collapse the grace window and `Refunded` healthy, still-trading launches out
/// from under graduation. 1 hour is short enough not to constrain real incident
/// response (the keeper can refund a stuck launch at any time) but blocks the
/// instant-mass-refund foot-gun. Enforced at instantiate and `UpdateRefundDeadline`.
pub const MIN_REFUND_DEADLINE_SECONDS: u64 = 3600;

/// Injective contract-address width. `cosmwasm_std::instantiate2_address`
/// returns the full 32-byte wasmd address hash, but Injective (an Ethermint
/// fork) truncates every contract/account address to the first 20 bytes — so
/// the on-chain `seeder_addr` derivation slices the hash here. Mirrors
/// `chain_capability_harness::instantiate2_addr` and the keeper's
/// `addressing/instantiate2.ts`, both proven against the real chain by the
/// `chain_capability_harness` integration tests.
const INJ_ADDR_LEN: usize = 20;

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
    info: MessageInfo,
    msg: InstantiateMsg,
) -> Result<Response<InjectiveMsgWrapper>, ContractError> {
    set_contract_version(deps.storage, CONTRACT_NAME, CONTRACT_VERSION)?;

    // Instantiate accepts no funds. The CreateDenom fee is paid per-launch at
    // RegisterLaunch, never here — any attached coin would be silently absorbed
    // into the contract balance with no recovery path, so reject up front.
    if !info.funds.is_empty() {
        return Err(ContractError::InstantiateDoesNotAcceptFunds {});
    }

    validate_subdenom_prefix(&msg.subdenom_prefix)?;
    if msg.decimals > MAX_DECIMALS {
        return Err(ContractError::DecimalsOutOfRange { got: msg.decimals });
    }
    if msg.refund_deadline_seconds < MIN_REFUND_DEADLINE_SECONDS {
        return Err(ContractError::RefundDeadlineTooShort {
            got: msg.refund_deadline_seconds,
            min: MIN_REFUND_DEADLINE_SECONDS,
        });
    }

    let admin = deps.api.addr_validate(&msg.admin)?;
    // The admin governs the issuer via a two-step handoff (UpdateAdmin/
    // AcceptAdmin). Setting it to the tokenfactory dead-burn address at deploy
    // would permanently brick governance (no key can ever AcceptAdmin), so
    // reject the obvious foot-gun. Rotation to the dead address is still
    // possible later only through the denom-admin renounce path, not here.
    if admin.as_str() == DEAD_TOKENFACTORY_ADMIN {
        return Err(ContractError::AdminIsDeadAddress {});
    }
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
            // Secure-by-default: derive + verify the sink address on-chain. The
            // admin can disable via `SetVerifySeederDerivation` if it ever needs
            // to be turned off without a redeploy.
            verify_seeder_derivation: true,
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
        ExecuteMsg::SetVerifySeederDerivation { enabled } => {
            execute_set_verify_seeder_derivation(deps, info, enabled)
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
    // Layer A entropy is MANDATORY (fix for M-2 address-squat DoS). Require a
    // sufficiently-long, keeper-chosen, ASCII-alphanumeric `salt_suffix`: it is
    // folded into BOTH the subdenom (here) AND the sink/locker `Instantiate2`
    // salts (the derivation checks below), so the launch denom and the
    // per-launch sink + locker addresses are all unguessable until this tx
    // exists. Without it, the sequential `internal_id` + public issuer address
    // make every future sink/locker address pre-computable, and an attacker can
    // pre-`Instantiate2` the canonical address so that `RegisterLaunch`'s own
    // sink/locker creation collides and reverts the whole launch (permanent,
    // cheap, wholesale DoS). The keeper persists the salt so the denom
    // round-trips; off-chain consumers read it from the `register_launch` event.
    let salt = salt_suffix
        .as_deref()
        .ok_or(ContractError::SaltSuffixRequired {})?;
    if salt.len() < MIN_SALT_SUFFIX_LEN || !salt.chars().all(|c| c.is_ascii_alphanumeric()) {
        return Err(ContractError::SaltSuffixInvalid {
            got: salt.to_string(),
        });
    }
    let subdenom = format!("{}_{}_{}", config.subdenom_prefix, internal_id, salt);
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
    let (sink_init, sink_salt) =
        match cosmwasm_std::from_json::<SeederExecuteMsg>(&create_sink_payload)
            .map_err(|e| ContractError::SinkPayloadDecode(e.to_string()))?
        {
            // Capture `salt` too (P1): it is the exact salt the factory will use
            // in its `Instantiate2` when this same payload is forwarded (step 6),
            // so deriving the sink address from it is collision-free.
            SeederExecuteMsg::CreateSink { sink_init, salt } => (sink_init, salt),
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

    // H-1 (this session): pin the sink's failure-path pair-asset recipient. For
    // EVERY issuer-driven launch (not just SHROOM) `refund_receiver` MUST equal
    // the launch's `evm_authority` — the launch's controlling contract, which is
    // the natural failure-path refund authority (for SHROOM that's the
    // `LaunchpadCore` running proportional per-participant refunds). The
    // sink-address derivation below proves the sink is the real per-launch sink
    // but does NOT constrain the sink's init contents, so without this a
    // compromised keeper could register a valid-looking launch whose sink routes
    // any failure-path pair-asset (real value) to an attacker. (Driving the
    // seeder's `CreateSink` directly — bypassing the issuer — leaves
    // `refund_receiver` unconstrained; this invariant is issuer-enforced only.)
    let supplied_refund_receiver = deps.api.addr_validate(&sink_init.refund_receiver)?;
    if supplied_refund_receiver != evm_authority {
        return Err(ContractError::RefundReceiverMismatch {
            supplied: sink_init.refund_receiver.clone(),
            expected: evm_authority.to_string(),
        });
    }

    // Capture the CLMM `position_recipient` (the eventual owner of the locked-LP
    // position NFT) so the verification block below can pin it to the derived
    // locker. `None` for XYK launches (unreachable on the live SHROOM path).
    let clmm_position_recipient: Option<Addr> = match &sink_init.pool_kind {
        PoolKind::Clmm {
            clmm_factory: sink_clmm_factory,
            fee_tier,
            position_recipient,
            ..
        } => {
            let auth = clmm_pool_auth
                .as_ref()
                .ok_or(ContractError::ClmmAuthRequired {})?;
            // S-3: the pool reservation (Layer B) must NOT expire — a non-zero
            // TTL would let the `AuthorizeCreation` lapse between RegisterLaunch
            // and Settle, re-opening the CLMM pool-squat window (M-1) the
            // reservation exists to close. The launch denom is unique, so a
            // permanent (ttl=0) reservation can never block a legitimate pool.
            if auth.ttl_seconds != 0 {
                return Err(ContractError::ClmmAuthTtlMustBeZero {
                    got: auth.ttl_seconds,
                });
            }
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
            Some(deps.api.addr_validate(position_recipient)?)
        }
        PoolKind::Xyk { .. } => {
            if clmm_pool_auth.is_some() {
                return Err(ContractError::ClmmAuthUnexpected {});
            }
            None
        }
    };

    // P1 (this session): stop trusting the keeper-supplied `seeder_addr`. Derive
    // the sink's `Instantiate2` address on-chain from the seeder factory's live
    // `sink_code_id` checksum + the salt the factory will actually use, and
    // reject a mismatch. Until now `DeliverToSeeder` only bound the destination
    // to this launch's denom/pair (blocking accidental/cross-launch misroutes),
    // which a fully-compromised keeper could satisfy with a denom-matching
    // look-alike sink it controls — letting it capture `cw_held` AND, for a CLMM
    // launch, the locked-LP position minted to the `AuthorizeCreation` `creator`
    // (= `seeder_addr`) below. Deriving the address closes that vector. The
    // derivation MUST run before the CLMM `AuthorizeCreation` (step 5b) and the
    // `CreateSink` forward (step 6), both of which trust `seeder_addr`.
    if config.verify_seeder_derivation {
        // Layer A: the forwarded `CreateSink` salt MUST be the canonical entropic
        // sink salt, so the factory `Instantiate2`s the sink at the unguessable
        // address rather than a predictable one a squatter could have pre-
        // occupied. Asserting the exact salt (not just that the derived address
        // matches `seeder_addr`) is what forces the entropy through to the
        // address the factory actually creates.
        let expected_sink_salt = sink_salt_bytes(deps.as_ref(), &env, internal_id, salt)?;
        if sink_salt.as_slice() != expected_sink_salt.as_slice() {
            return Err(ContractError::SinkSaltMismatch {});
        }
        verify_seeder_addr_derivation(deps.as_ref(), &seeder_factory, &seeder_addr, &sink_salt)?;
    }

    // H-1 / LOW-1 (this session): pin a CLMM launch's `position_recipient` to the
    // derived per-launch locker UNCONDITIONALLY — even during a
    // `verify_seeder_derivation` escape-hatch window — so the locked-LP position
    // NFT can only ever mint to a no-withdraw locker, never to a keeper-chosen
    // address. The locker salt shares the same `salt_suffix` entropy as the sink.
    //
    // Why outside the `verify_seeder_derivation` block (unlike before, like the
    // `refund_receiver` pin above): the EVM-side sink-derivation arming pins the
    // sink ADDRESS but cannot see this CW-side `position_recipient` field, so an
    // unpinned recipient is an LP-theft vector a compromised keeper can reach even
    // with the EVM core armed. Fund-safety here outweighs the escape hatch's
    // ability to bypass a broken locker derivation — if the (shared) factory-
    // checksum derivation were itself broken you would redeploy, not run launches.
    if let Some(ref position_recipient) = clmm_position_recipient {
        verify_locker_addr_derivation(
            deps.as_ref(),
            &env,
            &seeder_factory,
            position_recipient,
            internal_id,
            salt,
        )?;
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
        // M-3: snapshot whether the sink/locker derivation actually ran here, so
        // `DeliverToSeeder` can refuse to ship to a sink that was registered
        // during a derivation-disabled escape-hatch window.
        seeder_verified: config.verify_seeder_derivation,
    };
    LAUNCHES.save(deps.storage, (&evm_authority, internal_id), &record)?;
    // Reverse index for `LaunchByDenom`. The denom is globally unique (it embeds
    // this issuer + the per-authority id + the Layer A salt), so this never
    // collides; the duplicate-`internal_id` guard above already rejected a
    // re-register before we reach here.
    DENOM_INDEX.save(deps.storage, &denom, &(evm_authority.clone(), internal_id))?;

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

    // M-3: never ship `cw_held` to a sink whose Instantiate2 address was not
    // derivation-verified at RegisterLaunch. `verify_seeder_derivation` is an
    // admin escape hatch that can be toggled off then back on; this stops the
    // window where a launch registered with the check OFF (its `seeder_addr`
    // unproven) could still deliver once the check is flipped back ON. Delivery
    // is allowed only if the record was verified OR verification is currently
    // disabled (the operator is knowingly running the escape hatch end-to-end).
    if config.verify_seeder_derivation && !record.seeder_verified {
        return Err(ContractError::SeederDerivationNotVerified {});
    }

    // Finding C-M3 + P2-B: refuse to bank-send `cw_held` unless `seeder_addr`
    // is provably the SINK for THIS launch. Querying its `SinkConfig` both
    // closes the old ghost/EOA case (a non-sink errors the query) AND binds the
    // destination to this launch's `denom`/`pair_denom`, so a keeper that
    // fat-fingers (or maliciously supplies) a wrong `seeder_addr` can no longer
    // strand `cw_held` at an unrelated contract or a different launch's sink —
    // both checks the bare contract-existence guard missed. (A denom-matching
    // look-alike sink is still possible under full keeper compromise; the
    // on-chain instantiate2 derivation would close that, at the cost of binding
    // this critical path to the factory's code checksum.)
    if !record.cw_held.is_zero() {
        let sink_cfg: SeederSinkConfig = deps
            .querier
            .query_wasm_smart(record.seeder_addr.as_str(), &SeederQueryMsg::SinkConfig {})
            .map_err(|_| ContractError::SeederAddrNotAContract {
                addr: record.seeder_addr.to_string(),
            })?;
        if sink_cfg.token_denom != record.denom {
            return Err(ContractError::SeederSinkConfigMismatch {
                addr: record.seeder_addr.to_string(),
                reason: format!(
                    "sink token_denom `{}` != launch denom `{}`",
                    sink_cfg.token_denom, record.denom
                ),
            });
        }
        if sink_cfg.pair_denom != record.pair_denom {
            return Err(ContractError::SeederSinkConfigMismatch {
                addr: record.seeder_addr.to_string(),
                reason: format!(
                    "sink pair_denom `{}` != launch pair_denom `{}`",
                    sink_cfg.pair_denom, record.pair_denom
                ),
            });
        }
        // S-6: defence-in-depth for the derivation-escape-hatch trust model.
        // When `verify_seeder_derivation` is OFF, the address derivation never
        // ran, so the denom-match checks above are the only gate — and a
        // denom-matching look-alike sink controlled by an attacker would pass.
        // Also assert the sink names THIS issuer as its `issuer`, so even in the
        // escape-hatch window `cw_held` can only ship to a sink that was created
        // to be fed by us.
        if sink_cfg.issuer != env.contract.address.as_str() {
            return Err(ContractError::SeederSinkConfigMismatch {
                addr: record.seeder_addr.to_string(),
                reason: format!(
                    "sink issuer `{}` != this issuer `{}`",
                    sink_cfg.issuer, env.contract.address
                ),
            });
        }
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
    if info.sender == config.keeper {
        // M-1: the `paused` breaker now ALSO halts keeper-initiated refunds.
        // `RefundFailedLaunch` is `Registered`-only and a launch stays
        // `Registered` for its whole curve lifetime, so a compromised keeper
        // could otherwise burn `cw_held` + the EVM supply of every in-flight
        // launch in one sweep. Pausing freezes that path; the admin's
        // post-deadline path below stays OPEN even while paused so genuine
        // failed launches can still be wound down by the timelock.
        if config.paused {
            return Err(ContractError::KeeperRefundPaused {});
        }
    } else {
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
    if new_refund_deadline_seconds < MIN_REFUND_DEADLINE_SECONDS {
        return Err(ContractError::RefundDeadlineTooShort {
            got: new_refund_deadline_seconds,
            min: MIN_REFUND_DEADLINE_SECONDS,
        });
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

/// Admin-only (P1): flip the on-chain `seeder_addr` Instantiate2-derivation
/// check. Disabling it reverts `RegisterLaunch` to the v1 trust model (it trusts
/// the keeper-supplied address; `DeliverToSeeder`'s sink denom-match guard still
/// applies). Provided as a mainnet escape hatch so an unforeseen issue with the
/// derivation can't brick every launch without recourse to a redeploy.
fn execute_set_verify_seeder_derivation(
    deps: DepsMut<InjectiveQueryWrapper>,
    info: MessageInfo,
    enabled: bool,
) -> Result<Response<InjectiveMsgWrapper>, ContractError> {
    let mut config = CONFIG.load(deps.storage)?;
    if info.sender != config.admin {
        return Err(ContractError::Unauthorized {});
    }
    config.verify_seeder_derivation = enabled;
    CONFIG.save(deps.storage, &config)?;
    Ok(Response::new()
        .add_attribute("action", "set_verify_seeder_derivation")
        .add_attribute("verify_seeder_derivation", enabled.to_string()))
}

/// P1: derive the per-launch sink's `Instantiate2` address on-chain and assert
/// it equals the keeper-supplied `seeder_addr`.
///
/// The derivation inputs are exactly what the seeder factory will feed its own
/// `WasmMsg::Instantiate2` when `RegisterLaunch` forwards the `CreateSink`
/// payload (step 6) in the SAME tx:
///   * `checksum`  — the sha256 of the factory's live `sink_code_id` (queried
///     from its `FactoryConfig`, then `CodeInfo`). No TOCTOU: only the factory
///     admin's `UpdateSinkCodeId` can change it and there is no reentrancy that
///     could fire mid-tx.
///   * `creator`   — the seeder factory itself (it is the `Instantiate2` caller).
///   * `salt`      — the literal salt bytes from the `CreateSink` payload (NOT a
///     keeper-claimed convention; the same payload bytes the factory consumes).
///
/// So the derived address is provably the sink the factory will create.
///
/// `cosmwasm_std::instantiate2_address` returns the full 32-byte wasmd hash;
/// Injective truncates contract addresses to the first [`INJ_ADDR_LEN`] bytes,
/// so we slice before comparing. The `chain_capability_harness` integration
/// tests run this exact path against the real chain (a register with the
/// harness-derived address must commit), which is what proves the truncation +
/// key construction match Injective's actual `Instantiate2`.
fn verify_seeder_addr_derivation(
    deps: Deps<InjectiveQueryWrapper>,
    seeder_factory: &Addr,
    seeder_addr: &Addr,
    salt: &Binary,
) -> Result<(), ContractError> {
    let (sink_code_id, checksum) = query_factory_sink_code(deps, seeder_factory)?;
    let derived20 =
        derive_factory_instantiate2_20(deps, &checksum, seeder_factory, salt.as_slice())?;

    let supplied_canon = deps.api.addr_canonicalize(seeder_addr.as_str())?;
    if derived20.as_slice() != supplied_canon.as_slice() {
        // Best-effort humanize of the derived address for the error message.
        let derived_human = deps
            .api
            .addr_humanize(&CanonicalAddr::from(derived20.as_slice()))
            .map(|a| a.to_string())
            .unwrap_or_else(|_| "<unhumanizable>".to_string());
        return Err(ContractError::SeederAddrDerivationMismatch {
            supplied: seeder_addr.to_string(),
            derived: derived_human,
            sink_code_id,
        });
    }
    Ok(())
}

/// H-1 (this session): pin a CLMM launch's `position_recipient` to the
/// deterministically-derived per-launch **Locker**.
///
/// The sink-address derivation above proves the funds land in the real sink,
/// but `Instantiate2`'s address is a function of `(creator, code_id, salt)`
/// ONLY — NOT of the init message — so it does NOT constrain the sink's stored
/// `position_recipient`. Left unchecked, a fully-compromised keeper could
/// register an otherwise-valid launch whose sink mints the full-range
/// graduation position NFT to an attacker address; the attacker then calls
/// `choice_clmm_manager::DecreaseLiquidity` (owner-gated) and drains the entire
/// graduation pool. A genuine Locker is the only recipient with NO
/// principal-withdrawal path (it can `CollectFees` but never decrease), which is
/// exactly what makes graduation liquidity permanently locked.
///
/// The keeper derives the locker address as
/// `instantiate2(seeder_factory, sink_code_id, salt = b"locker" ||
/// canonical(issuer) || be_u64(internal_id))` (see the keeper's
/// `addressing/instantiate2.ts::lockerSalt`) and creates it in the SAME tx,
/// immediately before `RegisterLaunch`. We reproduce that salt on-chain and
/// reject any `position_recipient` that isn't the derived locker. Whatever lives
/// at the derived address was necessarily instantiated by the seeder factory
/// from `sink_code_id` (the only contract code it deploys), so it has no
/// principal-withdrawal path regardless of its init.
fn verify_locker_addr_derivation(
    deps: Deps<InjectiveQueryWrapper>,
    env: &Env,
    seeder_factory: &Addr,
    position_recipient: &Addr,
    internal_id: u64,
    salt_suffix: &str,
) -> Result<(), ContractError> {
    let (_sink_code_id, checksum) = query_factory_sink_code(deps, seeder_factory)?;
    let locker_salt = locker_salt_bytes(deps, env, internal_id, salt_suffix)?;
    let derived20 = derive_factory_instantiate2_20(deps, &checksum, seeder_factory, &locker_salt)?;

    let supplied_canon = deps.api.addr_canonicalize(position_recipient.as_str())?;
    if derived20.as_slice() != supplied_canon.as_slice() {
        let derived_human = deps
            .api
            .addr_humanize(&CanonicalAddr::from(derived20.as_slice()))
            .map(|a| a.to_string())
            .unwrap_or_else(|_| "<unhumanizable>".to_string());
        return Err(ContractError::LockerAddrDerivationMismatch {
            supplied: position_recipient.to_string(),
            derived: derived_human,
        });
    }
    Ok(())
}

/// Per-launch sink `Instantiate2` salt, byte-identical to the keeper's
/// `issuerSalt`: `canonical(issuer) || be_u64(internal_id) || salt_suffix`.
///
/// Layer A (anti-squat entropy, fix for M-2): the `salt_suffix` is the same
/// high-entropy nonce folded into the subdenom. Folding it into the sink salt
/// makes the per-launch sink address unguessable until this `RegisterLaunch` tx
/// exists — and since the sink is `Instantiate2`'d *inside* this same tx, an
/// attacker can no longer pre-squat the canonical sink address (which would
/// otherwise collide and revert the whole launch). Entropy is mandatory; see
/// the suffix validation in `execute_register_launch`.
fn sink_salt_bytes(
    deps: Deps<InjectiveQueryWrapper>,
    env: &Env,
    internal_id: u64,
    salt_suffix: &str,
) -> Result<Vec<u8>, ContractError> {
    let issuer_canon = deps.api.addr_canonicalize(env.contract.address.as_str())?;
    let mut salt = Vec::with_capacity(issuer_canon.len() + 8 + salt_suffix.len());
    salt.extend_from_slice(issuer_canon.as_slice());
    salt.extend_from_slice(&internal_id.to_be_bytes());
    salt.extend_from_slice(salt_suffix.as_bytes());
    Ok(salt)
}

/// Locker `Instantiate2` salt, byte-identical to the keeper's `lockerSalt`:
/// `b"locker" || sink_salt_bytes(...)` =
/// `b"locker" || canonical(issuer) || be_u64(internal_id) || salt_suffix`. The
/// `"locker"` marker distinguishes it from the sink salt so the two derive to
/// different addresses off the same factory + code id; the shared `salt_suffix`
/// carries the Layer A entropy that makes the locker address un-squattable.
fn locker_salt_bytes(
    deps: Deps<InjectiveQueryWrapper>,
    env: &Env,
    internal_id: u64,
    salt_suffix: &str,
) -> Result<Vec<u8>, ContractError> {
    let mut salt = Vec::with_capacity(6);
    salt.extend_from_slice(b"locker");
    salt.extend_from_slice(&sink_salt_bytes(deps, env, internal_id, salt_suffix)?);
    Ok(salt)
}

/// Query the seeder factory's live `sink_code_id` and its code checksum — the
/// two reads both `Instantiate2` derivations (sink + locker) share. No TOCTOU:
/// only the factory admin's `UpdateSinkCodeId` mutates the code id and there is
/// no reentrancy that could fire mid-tx.
fn query_factory_sink_code(
    deps: Deps<InjectiveQueryWrapper>,
    seeder_factory: &Addr,
) -> Result<(u64, Vec<u8>), ContractError> {
    let factory_cfg: SeederFactoryConfig = deps
        .querier
        .query_wasm_smart(seeder_factory.as_str(), &SeederQueryMsg::FactoryConfig {})
        .map_err(|e| ContractError::SeederFactoryConfigQuery {
            addr: seeder_factory.to_string(),
            reason: e.to_string(),
        })?;
    let code_info = deps
        .querier
        .query_wasm_code_info(factory_cfg.sink_code_id)
        .map_err(|e| ContractError::SinkCodeInfoQuery {
            code_id: factory_cfg.sink_code_id,
            reason: e.to_string(),
        })?;
    Ok((
        factory_cfg.sink_code_id,
        code_info.checksum.as_slice().to_vec(),
    ))
}

/// Derive the 20-byte Injective `Instantiate2` address for
/// `(checksum, creator = seeder_factory, salt)`. `cosmwasm_std::instantiate2_address`
/// returns the full 32-byte wasmd hash; Injective truncates to the first
/// [`INJ_ADDR_LEN`] bytes, so we slice before returning the canonical form.
fn derive_factory_instantiate2_20(
    deps: Deps<InjectiveQueryWrapper>,
    checksum: &[u8],
    seeder_factory: &Addr,
    salt: &[u8],
) -> Result<Vec<u8>, ContractError> {
    let creator_canon = deps.api.addr_canonicalize(seeder_factory.as_str())?;
    let derived_full = instantiate2_address(checksum, &creator_canon, salt)
        .map_err(|e| ContractError::SeederAddrDerivation(e.to_string()))?;
    let derived20 = derived_full.as_slice().get(..INJ_ADDR_LEN).ok_or_else(|| {
        ContractError::SeederAddrDerivation(
            "derived address hash shorter than 20 bytes".to_string(),
        )
    })?;
    Ok(derived20.to_vec())
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
        QueryMsg::LaunchByDenom { denom } => to_json_binary(&query_launch_by_denom(deps, denom)?),
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
        verify_seeder_derivation: c.verify_seeder_derivation,
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

fn query_launch_by_denom(
    deps: Deps<InjectiveQueryWrapper>,
    denom: String,
) -> StdResult<LaunchResponse> {
    let (evm_authority, internal_id) = DENOM_INDEX
        .may_load(deps.storage, &denom)?
        .ok_or_else(|| cosmwasm_std::StdError::not_found(format!("launch for denom {}", denom)))?;
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
            // P2-A: accept an in-MAJOR patch against the CURRENT major, not a
            // hard-coded `1.`. The old `starts_with("1.")` bricked every patch
            // once the contract shipped a 2.x build (both variants rejected),
            // leaving no incident-patch path. `FromV1` still handles the
            // cross-major 1.x → current hop.
            let cur_major = parse_major(&current.version);
            let this_major = parse_major(CONTRACT_VERSION);
            if cur_major.is_none() || cur_major != this_major {
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

/// Parse the leading `major` component of a semver string (e.g. `"2.3.1"` →
/// `Some(2)`). Returns `None` if there is no parseable leading integer.
fn parse_major(version: &str) -> Option<u64> {
    version
        .split('.')
        .next()
        .and_then(|s| s.parse::<u64>().ok())
}
