use cosmwasm_std::{
    entry_point, to_json_binary, Addr, BankMsg, Binary, Coin, Deps, DepsMut, Env, Event, HexBinary,
    MessageInfo, Order, Response, StdError, StdResult, Storage, Timestamp, Uint128, Uint256,
};
use cw_storage_plus::Bound;

use crate::error::ContractError;
use crate::merkle;
use crate::msg::{
    CampaignResponse, ClaimEntry, ClaimMsg, ClaimableResponse, ClaimedResponse, ClaimsResponse,
    ExecuteMsg, FundingRequiredResponse, InitialRoot, InstantiateMsg, LiabilitiesResponse,
    MigrateMsg, QueryMsg,
};
use crate::state::{
    Campaign, Config, CAMPAIGNS, CAMPAIGN_SEQ, CLAIMED, CONFIG, CREATOR_CAMPAIGNS, LIABILITIES,
};

const CONTRACT_NAME: &str = "crates.io:choice-claim-drops";
const CONTRACT_VERSION: &str = env!("CARGO_PKG_VERSION");

const MAX_FEE_BPS: u16 = 1_000;
/// Winding down a perpetual campaign (expiry None -> Some) needs this much
/// notice so a creator can't set expiry to "now" and immediately claw back
/// funds a published root promised to recipients.
const MIN_WIND_DOWN_SECONDS: u64 = 7 * 24 * 60 * 60;
/// 2^64 leaves is beyond any realistic tree; longer proofs are malformed.
const MAX_PROOF_LEN: usize = 64;
/// `meta` is re-serialized on every claim (it rides on the Campaign struct), so
/// keep it small; large blobs belong off-chain behind `leaves_uri`.
const MAX_META_LEN: usize = 4_096;
const MAX_LEAVES_URI_LEN: usize = 512;

const DEFAULT_LIMIT: u32 = 30;
const MAX_LIMIT: u32 = 100;

#[entry_point]
pub fn instantiate(
    deps: DepsMut,
    _env: Env,
    info: MessageInfo,
    msg: InstantiateMsg,
) -> Result<Response, ContractError> {
    cw2::set_contract_version(deps.storage, CONTRACT_NAME, CONTRACT_VERSION)?;

    if msg.fee_bps > MAX_FEE_BPS {
        return Err(ContractError::FeeTooHigh { max: MAX_FEE_BPS });
    }
    let owner = match msg.owner {
        Some(o) => deps.api.addr_validate(&o)?,
        None => info.sender,
    };
    let fee_collector = match msg.fee_collector {
        Some(c) => deps.api.addr_validate(&c)?,
        None => owner.clone(),
    };

    CONFIG.save(
        deps.storage,
        &Config {
            owner,
            pending_owner: None,
            fee_bps: msg.fee_bps,
            fee_collector,
            paused: false,
        },
    )?;
    CAMPAIGN_SEQ.save(deps.storage, &0u64)?;

    Ok(Response::new().add_attribute("action", "instantiate"))
}

#[entry_point]
pub fn execute(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
    msg: ExecuteMsg,
) -> Result<Response, ContractError> {
    let config = CONFIG.load(deps.storage)?;

    // The emergency stop halts everything that moves funds or creates new
    // obligations. Campaign administration and ownership/config stay available
    // so the owner can un-pause and rotate keys.
    if config.paused {
        match &msg {
            ExecuteMsg::CreateCampaign { .. }
            | ExecuteMsg::UpdateRoot { .. }
            | ExecuteMsg::Claim { .. }
            | ExecuteMsg::ClaimMany { .. }
            | ExecuteMsg::Clawback { .. }
            | ExecuteMsg::Rescue { .. } => return Err(ContractError::Paused),
            _ => {}
        }
    }

    match msg {
        ExecuteMsg::CreateCampaign {
            denom,
            meta,
            keeper,
            expiry,
            streaming,
            initial,
        } => create_campaign(
            deps, env, info, &config, denom, meta, keeper, expiry, streaming, initial,
        ),
        ExecuteMsg::UpdateRoot {
            id,
            root,
            total,
            leaves_uri,
        } => update_root(deps, env, info, &config, id, root, total, leaves_uri),
        ExecuteMsg::Freeze { id } => freeze(deps, info, id),
        ExecuteMsg::Claim { id, amount, proof } => {
            let (payout, denom) = apply_claim(
                deps.storage,
                env.block.time,
                id,
                &info.sender,
                amount,
                &proof,
            )?;
            Ok(Response::new()
                .add_message(BankMsg::Send {
                    to_address: info.sender.to_string(),
                    amount: vec![Coin {
                        denom: denom.clone(),
                        amount: payout,
                    }],
                })
                .add_event(claim_event(id, &info.sender, amount, payout, &denom))
                .add_attribute("action", "claim"))
        }
        ExecuteMsg::ClaimMany {
            claims,
            allow_partial,
        } => claim_many(deps, env, info, claims, allow_partial.unwrap_or(false)),
        ExecuteMsg::Clawback { id } => clawback(deps, env, info, id),
        ExecuteMsg::Rescue {
            denom,
            amount,
            recipient,
        } => rescue(deps, env, info, &config, denom, amount, recipient),
        ExecuteMsg::TransferOwnership { new_owner } => {
            transfer_ownership(deps, info, config, new_owner)
        }
        ExecuteMsg::AcceptOwnership {} => accept_ownership(deps, info, config),
        ExecuteMsg::TransferCreator { id, new_creator } => {
            transfer_creator(deps, info, id, new_creator)
        }
        ExecuteMsg::AcceptCreator { id } => accept_creator(deps, info, id),
        ExecuteMsg::SetKeeper { id, keeper } => set_keeper(deps, info, id, keeper),
        ExecuteMsg::SetExpiry { id, expiry } => set_expiry(deps, env, info, id, expiry),
        ExecuteMsg::SetCampaignPaused { id, paused } => set_campaign_paused(deps, info, id, paused),
        ExecuteMsg::UpdateMeta {
            id,
            meta,
            leaves_uri,
        } => update_meta(deps, info, id, meta, leaves_uri),
        ExecuteMsg::UpdateConfig {
            fee_bps,
            fee_collector,
            paused,
        } => update_config(deps, info, config, fee_bps, fee_collector, paused),
    }
}

#[allow(clippy::too_many_arguments)]
fn create_campaign(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
    config: &Config,
    denom: String,
    meta: String,
    keeper: Option<String>,
    expiry: Option<Timestamp>,
    streaming: bool,
    initial: Option<InitialRoot>,
) -> Result<Response, ContractError> {
    if denom.trim().is_empty() {
        return Err(ContractError::InvalidDenom);
    }
    if meta.len() > MAX_META_LEN {
        return Err(ContractError::MetaTooLong { max: MAX_META_LEN });
    }
    // The permission split: only the owner may open a mutable-root (streaming)
    // campaign; everyone else gets a one-shot that auto-freezes on publish.
    if streaming && info.sender != config.owner {
        return Err(ContractError::StreamingRequiresOwner);
    }
    let keeper = keeper.map(|k| deps.api.addr_validate(&k)).transpose()?;
    if keeper.is_some() && !streaming {
        return Err(ContractError::KeeperRequiresStreaming);
    }
    if let Some(exp) = expiry {
        if exp <= env.block.time {
            return Err(ContractError::InvalidExpiry {
                reason: "expiry must be in the future".to_string(),
            });
        }
    }
    // Without an initial root the create carries no funds.
    if initial.is_none() && !info.funds.is_empty() {
        return Err(ContractError::UnexpectedFunds);
    }

    let id = CAMPAIGN_SEQ.load(deps.storage)? + 1;
    CAMPAIGN_SEQ.save(deps.storage, &id)?;

    let mut campaign = Campaign {
        creator: info.sender.clone(),
        pending_creator: None,
        keeper,
        streaming,
        denom: denom.clone(),
        meta,
        leaves_uri: String::new(),
        root: None,
        total: Uint128::zero(),
        prev_root: None,
        prev_total: Uint128::zero(),
        claimed_total: Uint128::zero(),
        claimants: 0,
        frozen: false,
        expiry,
        paused: false,
        swept: false,
    };
    CREATOR_CAMPAIGNS.save(deps.storage, (&info.sender, id), &())?;

    let mut res = Response::new()
        .add_attribute("action", "create_campaign")
        .add_attribute("id", id.to_string())
        .add_attribute("creator", info.sender.as_str())
        .add_attribute("denom", denom)
        .add_attribute("streaming", streaming.to_string())
        .add_attribute(
            "keeper",
            campaign
                .keeper
                .as_ref()
                .map(|k| k.to_string())
                .unwrap_or_else(|| "none".to_string()),
        )
        .add_attribute(
            "expiry",
            expiry
                .map(|e| e.seconds().to_string())
                .unwrap_or_else(|| "none".to_string()),
        );

    if let Some(init) = initial {
        let outcome = apply_root_update(
            deps.storage,
            &mut campaign,
            config,
            &info,
            init.root,
            init.total,
            init.leaves_uri,
            env.block.time,
        )?;
        // One-shot campaigns are immutable from their first publish.
        if !streaming {
            campaign.frozen = true;
        }
        res = res.add_event(outcome.event(id));
        if let Some(msg) = outcome.fee_msg {
            res = res.add_message(msg);
        }
    }

    CAMPAIGNS.save(deps.storage, id, &campaign)?;
    Ok(res.set_data(to_json_binary(&id)?))
}

#[allow(clippy::too_many_arguments)]
fn update_root(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
    config: &Config,
    id: u64,
    root: HexBinary,
    total: Uint128,
    leaves_uri: String,
) -> Result<Response, ContractError> {
    let mut campaign = load_campaign(deps.storage, id)?;

    let is_keeper = campaign.keeper.as_ref() == Some(&info.sender);
    if info.sender != campaign.creator && !is_keeper {
        return Err(ContractError::Unauthorized);
    }

    let outcome = apply_root_update(
        deps.storage,
        &mut campaign,
        config,
        &info,
        root,
        total,
        leaves_uri,
        env.block.time,
    )?;
    // A one-shot gets exactly one root; auto-freeze so it can never be replaced.
    if !campaign.streaming {
        campaign.frozen = true;
    }
    let event = outcome.event(id);
    CAMPAIGNS.save(deps.storage, id, &campaign)?;

    let mut res = Response::new().add_event(event);
    if let Some(msg) = outcome.fee_msg {
        res = res.add_message(msg);
    }
    Ok(res)
}

struct RootOutcome {
    fee_msg: Option<BankMsg>,
    root_hex: String,
    total: Uint128,
    delta: Uint128,
    fee: Uint128,
}

impl RootOutcome {
    fn event(&self, id: u64) -> Event {
        Event::new("update_root")
            .add_attribute("id", id.to_string())
            .add_attribute("root", &self.root_hex)
            .add_attribute("total", self.total)
            .add_attribute("delta", self.delta)
            .add_attribute("fee", self.fee)
    }
}

/// Core of publishing a root: validates, enforces the exact-funding solvency
/// invariant, rotates the previous root, books the liability, and returns the
/// optional fee transfer. Shared by `CreateCampaign { initial }` and
/// `UpdateRoot`. Does NOT check authorization or set `frozen` — callers do.
#[allow(clippy::too_many_arguments)]
fn apply_root_update(
    storage: &mut dyn Storage,
    campaign: &mut Campaign,
    config: &Config,
    info: &MessageInfo,
    root: HexBinary,
    total: Uint128,
    leaves_uri: String,
    now: Timestamp,
) -> Result<RootOutcome, ContractError> {
    if campaign.swept {
        return Err(ContractError::Swept);
    }
    if campaign.frozen {
        return Err(ContractError::Frozen);
    }
    if campaign.is_expired(now) {
        return Err(ContractError::Expired);
    }
    if root.len() != 32 {
        return Err(ContractError::InvalidRoot);
    }
    if leaves_uri.len() > MAX_LEAVES_URI_LEN {
        return Err(ContractError::LeavesUriTooLong {
            max: MAX_LEAVES_URI_LEN,
        });
    }
    if total < campaign.total {
        return Err(ContractError::TotalDecreased {
            current: campaign.total,
            new: total,
        });
    }

    let delta = total.checked_sub(campaign.total).map_err(StdError::from)?;
    // Fee is charged on top so the campaign always retains exactly `delta` and
    // stays solvent for its declared total. Ceil so tiny deltas can't dodge it.
    let fee = ceil_fee(delta, config.fee_bps)?;
    let required = delta.checked_add(fee).map_err(StdError::from)?;
    assert_exact_funds(info, &campaign.denom, required)?;

    if campaign.root.is_some() {
        campaign.prev_root = campaign.root.clone();
        campaign.prev_total = campaign.total;
    }
    let root_hex = root.to_hex();
    campaign.root = Some(root);
    campaign.total = total;
    campaign.leaves_uri = leaves_uri;

    add_liability(storage, &campaign.denom, delta)?;

    let fee_msg = if fee.is_zero() {
        None
    } else {
        Some(BankMsg::Send {
            to_address: config.fee_collector.to_string(),
            amount: vec![Coin {
                denom: campaign.denom.clone(),
                amount: fee,
            }],
        })
    };

    Ok(RootOutcome {
        fee_msg,
        root_hex,
        total,
        delta,
        fee,
    })
}

fn freeze(deps: DepsMut, info: MessageInfo, id: u64) -> Result<Response, ContractError> {
    let mut campaign = load_campaign(deps.storage, id)?;
    if info.sender != campaign.creator {
        return Err(ContractError::Unauthorized);
    }
    if campaign.root.is_none() {
        return Err(ContractError::NoRoot);
    }
    campaign.frozen = true;
    CAMPAIGNS.save(deps.storage, id, &campaign)?;
    Ok(Response::new()
        .add_attribute("action", "freeze")
        .add_attribute("id", id.to_string()))
}

/// Shared claim core for Claim / ClaimMany. Verifies the proof against the
/// current root, falling back to the previous one, pays the cumulative delta,
/// and hard-caps every payout at the campaign's remaining declared funds so a
/// dishonest tree can never touch another campaign's balance.
fn apply_claim(
    storage: &mut dyn Storage,
    now: Timestamp,
    id: u64,
    sender: &Addr,
    amount: Uint128,
    proof: &[HexBinary],
) -> Result<(Uint128, String), ContractError> {
    let mut campaign = load_campaign(storage, id)?;
    if campaign.paused {
        return Err(ContractError::CampaignPaused);
    }
    if campaign.swept {
        return Err(ContractError::Swept);
    }
    if campaign.is_expired(now) {
        return Err(ContractError::Expired);
    }
    if proof.len() > MAX_PROOF_LEN {
        return Err(ContractError::ProofTooLong);
    }

    let leaf = merkle::leaf_hash(sender.as_str(), amount);
    let valid = campaign
        .root
        .as_ref()
        .is_some_and(|r| merkle::verify(r.as_slice(), leaf, proof))
        || campaign
            .prev_root
            .as_ref()
            .is_some_and(|r| merkle::verify(r.as_slice(), leaf, proof));
    if !valid {
        return Err(ContractError::InvalidProof);
    }

    let claimed = CLAIMED.may_load(storage, (id, sender))?.unwrap_or_default();
    if amount <= claimed {
        return Err(ContractError::NothingToClaim);
    }
    let payout = amount.checked_sub(claimed).map_err(StdError::from)?;
    if payout > campaign.remaining() {
        return Err(ContractError::ExceedsCampaignFunds);
    }

    if claimed.is_zero() {
        campaign.claimants += 1;
    }
    CLAIMED.save(storage, (id, sender), &amount)?;
    campaign.claimed_total = campaign
        .claimed_total
        .checked_add(payout)
        .map_err(StdError::from)?;
    let denom = campaign.denom.clone();
    CAMPAIGNS.save(storage, id, &campaign)?;
    sub_liability(storage, &denom, payout)?;
    Ok((payout, denom))
}

fn claim_many(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
    claims: Vec<ClaimMsg>,
    allow_partial: bool,
) -> Result<Response, ContractError> {
    if claims.is_empty() {
        return Err(ContractError::NothingToClaim);
    }
    let mut res = Response::new()
        .add_attribute("action", "claim_many")
        .add_attribute("claimant", info.sender.as_str());
    let mut paid_any = false;
    for c in claims {
        match apply_claim(
            deps.storage,
            env.block.time,
            c.id,
            &info.sender,
            c.amount,
            &c.proof,
        ) {
            Ok((payout, denom)) => {
                paid_any = true;
                res = res
                    .add_message(BankMsg::Send {
                        to_address: info.sender.to_string(),
                        amount: vec![Coin {
                            denom: denom.clone(),
                            amount: payout,
                        }],
                    })
                    .add_event(claim_event(c.id, &info.sender, c.amount, payout, &denom));
            }
            Err(e) if allow_partial => {
                res = res.add_event(
                    Event::new("claim_skipped")
                        .add_attribute("id", c.id.to_string())
                        .add_attribute("reason", e.to_string()),
                );
            }
            Err(e) => return Err(e),
        }
    }
    if !paid_any {
        return Err(ContractError::NothingToClaim);
    }
    Ok(res)
}

fn clawback(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
    id: u64,
) -> Result<Response, ContractError> {
    let mut campaign = load_campaign(deps.storage, id)?;
    if info.sender != campaign.creator {
        return Err(ContractError::Unauthorized);
    }
    if campaign.swept {
        return Err(ContractError::Swept);
    }
    if !campaign.is_expired(env.block.time) {
        return Err(ContractError::NotExpired);
    }

    let remaining = campaign.remaining();
    campaign.swept = true;
    let denom = campaign.denom.clone();
    CAMPAIGNS.save(deps.storage, id, &campaign)?;
    sub_liability(deps.storage, &denom, remaining)?;

    let mut res = Response::new()
        .add_attribute("action", "clawback")
        .add_attribute("id", id.to_string())
        .add_attribute("amount", remaining);
    if !remaining.is_zero() {
        res = res.add_message(BankMsg::Send {
            to_address: campaign.creator.to_string(),
            amount: vec![Coin {
                denom,
                amount: remaining,
            }],
        });
    }
    Ok(res)
}

fn rescue(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
    config: &Config,
    denom: String,
    amount: Option<Uint128>,
    recipient: Option<String>,
) -> Result<Response, ContractError> {
    if info.sender != config.owner {
        return Err(ContractError::Unauthorized);
    }
    let balance = deps
        .querier
        .query_balance(&env.contract.address, &denom)?
        .amount;
    let owed = LIABILITIES
        .may_load(deps.storage, &denom)?
        .unwrap_or_default();
    // Everything above what claimants are owed is unencumbered.
    let excess = balance.saturating_sub(owed);
    let send = match amount {
        Some(a) => {
            if a > excess {
                return Err(ContractError::RescueExceedsExcess { available: excess });
            }
            a
        }
        None => excess,
    };
    if send.is_zero() {
        return Err(ContractError::NothingToRescue);
    }
    let to = match recipient {
        Some(r) => deps.api.addr_validate(&r)?,
        None => config.owner.clone(),
    };
    Ok(Response::new()
        .add_message(BankMsg::Send {
            to_address: to.to_string(),
            amount: vec![Coin {
                denom: denom.clone(),
                amount: send,
            }],
        })
        .add_attribute("action", "rescue")
        .add_attribute("denom", denom)
        .add_attribute("amount", send)
        .add_attribute("recipient", to))
}

fn transfer_ownership(
    deps: DepsMut,
    info: MessageInfo,
    mut config: Config,
    new_owner: String,
) -> Result<Response, ContractError> {
    if info.sender != config.owner {
        return Err(ContractError::Unauthorized);
    }
    let pending = deps.api.addr_validate(&new_owner)?;
    config.pending_owner = Some(pending.clone());
    CONFIG.save(deps.storage, &config)?;
    Ok(Response::new()
        .add_attribute("action", "transfer_ownership")
        .add_attribute("pending_owner", pending))
}

fn accept_ownership(
    deps: DepsMut,
    info: MessageInfo,
    mut config: Config,
) -> Result<Response, ContractError> {
    match &config.pending_owner {
        Some(p) if *p == info.sender => {}
        _ => return Err(ContractError::NoPendingOwnership),
    }
    config.owner = info.sender.clone();
    config.pending_owner = None;
    CONFIG.save(deps.storage, &config)?;
    Ok(Response::new()
        .add_attribute("action", "accept_ownership")
        .add_attribute("owner", info.sender))
}

fn transfer_creator(
    deps: DepsMut,
    info: MessageInfo,
    id: u64,
    new_creator: String,
) -> Result<Response, ContractError> {
    let mut campaign = load_campaign(deps.storage, id)?;
    if info.sender != campaign.creator {
        return Err(ContractError::Unauthorized);
    }
    let pending = deps.api.addr_validate(&new_creator)?;
    campaign.pending_creator = Some(pending.clone());
    CAMPAIGNS.save(deps.storage, id, &campaign)?;
    Ok(Response::new()
        .add_attribute("action", "transfer_creator")
        .add_attribute("id", id.to_string())
        .add_attribute("pending_creator", pending))
}

fn accept_creator(deps: DepsMut, info: MessageInfo, id: u64) -> Result<Response, ContractError> {
    let mut campaign = load_campaign(deps.storage, id)?;
    match &campaign.pending_creator {
        Some(p) if *p == info.sender => {}
        _ => return Err(ContractError::NoPendingCreator),
    }
    let old_creator = campaign.creator.clone();
    campaign.creator = info.sender.clone();
    campaign.pending_creator = None;
    CAMPAIGNS.save(deps.storage, id, &campaign)?;
    // Move the CampaignsByCreator index entry to the new owner.
    CREATOR_CAMPAIGNS.remove(deps.storage, (&old_creator, id));
    CREATOR_CAMPAIGNS.save(deps.storage, (&info.sender, id), &())?;
    Ok(Response::new()
        .add_attribute("action", "accept_creator")
        .add_attribute("id", id.to_string())
        .add_attribute("creator", info.sender))
}

fn set_keeper(
    deps: DepsMut,
    info: MessageInfo,
    id: u64,
    keeper: Option<String>,
) -> Result<Response, ContractError> {
    let mut campaign = load_campaign(deps.storage, id)?;
    if info.sender != campaign.creator {
        return Err(ContractError::Unauthorized);
    }
    if !campaign.streaming {
        return Err(ContractError::NotStreaming);
    }
    campaign.keeper = keeper.map(|k| deps.api.addr_validate(&k)).transpose()?;
    CAMPAIGNS.save(deps.storage, id, &campaign)?;
    Ok(Response::new()
        .add_attribute("action", "set_keeper")
        .add_attribute("id", id.to_string())
        .add_attribute(
            "keeper",
            campaign
                .keeper
                .as_ref()
                .map(|k| k.to_string())
                .unwrap_or_else(|| "none".to_string()),
        ))
}

fn set_expiry(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
    id: u64,
    expiry: Timestamp,
) -> Result<Response, ContractError> {
    let mut campaign = load_campaign(deps.storage, id)?;
    if info.sender != campaign.creator {
        return Err(ContractError::Unauthorized);
    }
    if campaign.swept {
        return Err(ContractError::Swept);
    }
    match campaign.expiry {
        // Recipients can rely on an announced window: it only ever extends.
        Some(current) => {
            if expiry <= current {
                return Err(ContractError::InvalidExpiry {
                    reason: "expiry can only be extended".to_string(),
                });
            }
        }
        None => {
            // A frozen perpetual drop promised no clawback; keep that promise.
            if campaign.frozen {
                return Err(ContractError::FrozenExpiryLocked);
            }
            if expiry < env.block.time.plus_seconds(MIN_WIND_DOWN_SECONDS) {
                return Err(ContractError::InvalidExpiry {
                    reason: "winding down a perpetual campaign requires at least 7 days notice"
                        .to_string(),
                });
            }
        }
    }
    campaign.expiry = Some(expiry);
    CAMPAIGNS.save(deps.storage, id, &campaign)?;
    Ok(Response::new()
        .add_attribute("action", "set_expiry")
        .add_attribute("id", id.to_string())
        .add_attribute("expiry", expiry.seconds().to_string()))
}

fn set_campaign_paused(
    deps: DepsMut,
    info: MessageInfo,
    id: u64,
    paused: bool,
) -> Result<Response, ContractError> {
    let mut campaign = load_campaign(deps.storage, id)?;
    if info.sender != campaign.creator {
        return Err(ContractError::Unauthorized);
    }
    campaign.paused = paused;
    CAMPAIGNS.save(deps.storage, id, &campaign)?;
    Ok(Response::new()
        .add_attribute("action", "set_campaign_paused")
        .add_attribute("id", id.to_string())
        .add_attribute("paused", paused.to_string()))
}

fn update_meta(
    deps: DepsMut,
    info: MessageInfo,
    id: u64,
    meta: Option<String>,
    leaves_uri: Option<String>,
) -> Result<Response, ContractError> {
    let mut campaign = load_campaign(deps.storage, id)?;
    if info.sender != campaign.creator {
        return Err(ContractError::Unauthorized);
    }
    let mut res = Response::new()
        .add_attribute("action", "update_meta")
        .add_attribute("id", id.to_string());
    if let Some(m) = meta {
        if m.len() > MAX_META_LEN {
            return Err(ContractError::MetaTooLong { max: MAX_META_LEN });
        }
        campaign.meta = m;
    }
    if let Some(u) = leaves_uri {
        if u.len() > MAX_LEAVES_URI_LEN {
            return Err(ContractError::LeavesUriTooLong {
                max: MAX_LEAVES_URI_LEN,
            });
        }
        res = res.add_attribute("leaves_uri", &u);
        campaign.leaves_uri = u;
    }
    CAMPAIGNS.save(deps.storage, id, &campaign)?;
    Ok(res)
}

fn update_config(
    deps: DepsMut,
    info: MessageInfo,
    mut config: Config,
    fee_bps: Option<u16>,
    fee_collector: Option<String>,
    paused: Option<bool>,
) -> Result<Response, ContractError> {
    if info.sender != config.owner {
        return Err(ContractError::Unauthorized);
    }
    let mut res = Response::new().add_attribute("action", "update_config");
    if let Some(bps) = fee_bps {
        if bps > MAX_FEE_BPS {
            return Err(ContractError::FeeTooHigh { max: MAX_FEE_BPS });
        }
        config.fee_bps = bps;
        res = res.add_attribute("fee_bps", bps.to_string());
    }
    if let Some(c) = fee_collector {
        config.fee_collector = deps.api.addr_validate(&c)?;
        res = res.add_attribute("fee_collector", c);
    }
    if let Some(p) = paused {
        config.paused = p;
        res = res.add_attribute("paused", p.to_string());
    }
    CONFIG.save(deps.storage, &config)?;
    Ok(res)
}

// ---- helpers ----

fn claim_event(
    id: u64,
    claimant: &Addr,
    cumulative: Uint128,
    payout: Uint128,
    denom: &str,
) -> Event {
    Event::new("claim")
        .add_attribute("id", id.to_string())
        .add_attribute("claimant", claimant.as_str())
        .add_attribute("cumulative", cumulative)
        .add_attribute("payout", payout)
        .add_attribute("denom", denom)
}

/// ceil(delta * fee_bps / 10_000). Ceil (not floor) so a large campaign can't
/// be funded as many tiny deltas that each round the platform fee to zero.
fn ceil_fee(delta: Uint128, fee_bps: u16) -> Result<Uint128, ContractError> {
    if fee_bps == 0 || delta.is_zero() {
        return Ok(Uint128::zero());
    }
    let denom = Uint256::from(10_000u128);
    let num = Uint256::from(delta)
        .checked_mul(Uint256::from(fee_bps as u128))
        .map_err(StdError::from)?;
    let ceil = num
        .checked_add(denom.checked_sub(Uint256::one()).map_err(StdError::from)?)
        .map_err(StdError::from)?
        .checked_div(denom)
        .map_err(StdError::from)?;
    Ok(Uint128::try_from(ceil).map_err(StdError::from)?)
}

fn add_liability(storage: &mut dyn Storage, denom: &str, amount: Uint128) -> StdResult<()> {
    LIABILITIES.update(storage, denom, |v| -> StdResult<_> {
        v.unwrap_or_default()
            .checked_add(amount)
            .map_err(Into::into)
    })?;
    Ok(())
}

fn sub_liability(storage: &mut dyn Storage, denom: &str, amount: Uint128) -> StdResult<()> {
    LIABILITIES.update(storage, denom, |v| -> StdResult<_> {
        v.unwrap_or_default()
            .checked_sub(amount)
            .map_err(Into::into)
    })?;
    Ok(())
}

fn load_campaign(storage: &dyn Storage, id: u64) -> Result<Campaign, ContractError> {
    CAMPAIGNS
        .may_load(storage, id)?
        .ok_or(ContractError::CampaignNotFound { id })
}

fn assert_exact_funds(
    info: &MessageInfo,
    denom: &str,
    required: Uint128,
) -> Result<(), ContractError> {
    if required.is_zero() {
        if info.funds.is_empty() {
            return Ok(());
        }
        return Err(ContractError::UnexpectedFunds);
    }
    match info.funds.as_slice() {
        [coin] if coin.denom == denom && coin.amount == required => Ok(()),
        _ => Err(ContractError::InvalidFunds {
            expected: required,
            denom: denom.to_string(),
        }),
    }
}

#[entry_point]
pub fn query(deps: Deps, env: Env, msg: QueryMsg) -> StdResult<Binary> {
    match msg {
        QueryMsg::Config {} => to_json_binary(&CONFIG.load(deps.storage)?),
        QueryMsg::Campaign { id } => {
            let campaign = CAMPAIGNS.load(deps.storage, id)?;
            to_json_binary(&CampaignResponse {
                id,
                remaining: campaign.remaining(),
                campaign,
            })
        }
        QueryMsg::Campaigns { start_after, limit } => {
            let limit = limit.unwrap_or(DEFAULT_LIMIT).min(MAX_LIMIT) as usize;
            let start = start_after.map(Bound::exclusive);
            let out: StdResult<Vec<_>> = CAMPAIGNS
                .range(deps.storage, start, None, Order::Ascending)
                .take(limit)
                .map(|item| {
                    let (id, campaign) = item?;
                    Ok(CampaignResponse {
                        id,
                        remaining: campaign.remaining(),
                        campaign,
                    })
                })
                .collect();
            to_json_binary(&out?)
        }
        QueryMsg::CampaignsByCreator {
            creator,
            start_after,
            limit,
        } => {
            let creator = deps.api.addr_validate(&creator)?;
            let limit = limit.unwrap_or(DEFAULT_LIMIT).min(MAX_LIMIT) as usize;
            let start = start_after.map(Bound::exclusive);
            let ids: StdResult<Vec<u64>> = CREATOR_CAMPAIGNS
                .prefix(&creator)
                .keys(deps.storage, start, None, Order::Ascending)
                .take(limit)
                .collect();
            let out: StdResult<Vec<_>> = ids?
                .into_iter()
                .map(|id| {
                    let campaign = CAMPAIGNS.load(deps.storage, id)?;
                    Ok(CampaignResponse {
                        id,
                        remaining: campaign.remaining(),
                        campaign,
                    })
                })
                .collect();
            to_json_binary(&out?)
        }
        QueryMsg::Claimed { id, address } => {
            let addr = deps.api.addr_validate(&address)?;
            let claimed = CLAIMED
                .may_load(deps.storage, (id, &addr))?
                .unwrap_or_default();
            to_json_binary(&ClaimedResponse { claimed })
        }
        QueryMsg::Claims {
            id,
            start_after,
            limit,
        } => {
            let limit = limit.unwrap_or(DEFAULT_LIMIT).min(MAX_LIMIT) as usize;
            let start = start_after
                .map(|s| deps.api.addr_validate(&s))
                .transpose()?;
            let bound = start.as_ref().map(Bound::exclusive);
            let claims: StdResult<Vec<ClaimEntry>> = CLAIMED
                .prefix(id)
                .range(deps.storage, bound, None, Order::Ascending)
                .take(limit)
                .map(|item| {
                    let (address, claimed) = item?;
                    Ok(ClaimEntry { address, claimed })
                })
                .collect();
            to_json_binary(&ClaimsResponse { claims: claims? })
        }
        QueryMsg::Claimable {
            id,
            address,
            amount,
            proof,
        } => {
            let addr = deps.api.addr_validate(&address)?;
            let campaign = CAMPAIGNS.load(deps.storage, id)?;
            let leaf = merkle::leaf_hash(addr.as_str(), amount);
            let valid = proof.len() <= MAX_PROOF_LEN
                && (campaign
                    .root
                    .as_ref()
                    .is_some_and(|r| merkle::verify(r.as_slice(), leaf, &proof))
                    || campaign
                        .prev_root
                        .as_ref()
                        .is_some_and(|r| merkle::verify(r.as_slice(), leaf, &proof)));
            let config = CONFIG.load(deps.storage)?;
            let open = valid
                && !config.paused
                && !campaign.paused
                && !campaign.swept
                && !campaign.is_expired(env.block.time);
            let payable = if open {
                let claimed = CLAIMED
                    .may_load(deps.storage, (id, &addr))?
                    .unwrap_or_default();
                amount
                    .checked_sub(claimed)
                    .unwrap_or_default()
                    .min(campaign.remaining())
            } else {
                Uint128::zero()
            };
            to_json_binary(&ClaimableResponse { valid, payable })
        }
        QueryMsg::FundingRequired { id, new_total } => {
            let campaign = CAMPAIGNS.load(deps.storage, id)?;
            let delta = new_total.checked_sub(campaign.total).unwrap_or_default();
            let fee = ceil_fee(delta, CONFIG.load(deps.storage)?.fee_bps)
                .map_err(|e| StdError::generic_err(e.to_string()))?;
            let required = delta.checked_add(fee)?;
            to_json_binary(&FundingRequiredResponse {
                delta,
                fee,
                required,
                denom: campaign.denom,
            })
        }
        QueryMsg::Liabilities { denom } => {
            let owed = LIABILITIES
                .may_load(deps.storage, &denom)?
                .unwrap_or_default();
            to_json_binary(&LiabilitiesResponse { owed })
        }
    }
}

#[entry_point]
pub fn migrate(deps: DepsMut, _env: Env, _msg: MigrateMsg) -> Result<Response, ContractError> {
    let stored = cw2::get_contract_version(deps.storage)?;
    if stored.contract != CONTRACT_NAME {
        return Err(ContractError::Std(StdError::generic_err(format!(
            "cannot migrate from {}",
            stored.contract
        ))));
    }
    // Refuse downgrades so a rollback can't silently reintroduce fixed bugs.
    if let (Some(from), Some(to)) = (
        parse_semver(&stored.version),
        parse_semver(CONTRACT_VERSION),
    ) {
        if to < from {
            return Err(ContractError::Std(StdError::generic_err(format!(
                "cannot migrate down from {} to {}",
                stored.version, CONTRACT_VERSION
            ))));
        }
    }
    cw2::set_contract_version(deps.storage, CONTRACT_NAME, CONTRACT_VERSION)?;
    Ok(Response::new()
        .add_attribute("action", "migrate")
        .add_attribute("from", stored.version)
        .add_attribute("to", CONTRACT_VERSION))
}

fn parse_semver(v: &str) -> Option<(u64, u64, u64)> {
    let mut it = v.split('.');
    let major = it.next()?.parse().ok()?;
    let minor = it.next()?.parse().ok()?;
    let patch = it.next()?.parse().ok()?;
    Some((major, minor, patch))
}
