use cosmwasm_std::testing::{
    message_info, mock_dependencies, mock_dependencies_with_balance, mock_env, MockApi,
    MockQuerier, MockStorage,
};
use cosmwasm_std::{
    coins, from_json, Addr, BankMsg, Coin, CosmosMsg, Env, HexBinary, OwnedDeps, SubMsg, Timestamp,
    Uint128,
};

use crate::contract::{execute, instantiate, migrate, query};
use crate::error::ContractError;
use crate::merkle;
use crate::msg::{
    CampaignResponse, ClaimEntry, ClaimMsg, ClaimableResponse, ClaimsResponse, ExecuteMsg,
    FundingRequiredResponse, InitialRoot, InstantiateMsg, LiabilitiesResponse, MigrateMsg,
    QueryMsg,
};
use crate::state::Config;

type Deps = OwnedDeps<MockStorage, MockApi, MockQuerier>;

const T0: u64 = 1_700_000_000;
const DENOM: &str = "factory/inj1creator/SAI";

// ---- test-side merkle builder (must mirror src/merkle.rs spec exactly) ----

fn hash_pair(a: &[u8; 32], b: &[u8; 32]) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    if a <= b {
        h.update(a);
        h.update(b);
    } else {
        h.update(b);
        h.update(a);
    }
    h.finalize().into()
}

/// Returns (root, proof per entry). Odd nodes promote unchanged.
fn build_tree(entries: &[(Addr, u128)]) -> (HexBinary, Vec<Vec<HexBinary>>) {
    let leaves: Vec<[u8; 32]> = entries
        .iter()
        .map(|(a, amt)| merkle::leaf_hash(a.as_str(), Uint128::new(*amt)))
        .collect();
    let mut levels: Vec<Vec<[u8; 32]>> = vec![leaves];
    while levels.last().unwrap().len() > 1 {
        let prev = levels.last().unwrap();
        let mut next = Vec::new();
        for pair in prev.chunks(2) {
            if pair.len() == 2 {
                next.push(hash_pair(&pair[0], &pair[1]));
            } else {
                next.push(pair[0]);
            }
        }
        levels.push(next);
    }
    let root = HexBinary::from(levels.last().unwrap()[0]);
    let proofs = (0..entries.len())
        .map(|leaf_idx| {
            let mut proof = Vec::new();
            let mut i = leaf_idx;
            for level in &levels[..levels.len() - 1] {
                let sib = i ^ 1;
                if sib < level.len() {
                    proof.push(HexBinary::from(level[sib]));
                }
                i /= 2;
            }
            proof
        })
        .collect();
    (root, proofs)
}

// ---- fixtures ----

fn init(deps: &mut Deps, fee_bps: u16, fee_collector: Option<String>) -> (Env, Addr) {
    let mut env = mock_env();
    env.block.time = Timestamp::from_seconds(T0);
    let owner = deps.api.addr_make("owner");
    instantiate(
        deps.as_mut(),
        env.clone(),
        message_info(&owner, &[]),
        InstantiateMsg {
            owner: None,
            fee_bps,
            fee_collector,
        },
    )
    .unwrap();
    (env, owner)
}

/// Create a one-shot (non-streaming) campaign, empty root.
fn create_one_shot(deps: &mut Deps, env: &Env, creator: &Addr, expiry: Option<Timestamp>) -> u64 {
    let res = execute(
        deps.as_mut(),
        env.clone(),
        message_info(creator, &[]),
        ExecuteMsg::CreateCampaign {
            denom: DENOM.to_string(),
            meta: "{\"title\":\"drop\"}".to_string(),
            keeper: None,
            expiry,
            streaming: false,
            initial: None,
        },
    )
    .unwrap();
    from_json::<u64>(res.data.unwrap()).unwrap()
}

/// Create a streaming campaign as `owner` with an optional keeper.
fn create_streaming(
    deps: &mut Deps,
    env: &Env,
    owner: &Addr,
    keeper: Option<String>,
    expiry: Option<Timestamp>,
) -> u64 {
    let res = execute(
        deps.as_mut(),
        env.clone(),
        message_info(owner, &[]),
        ExecuteMsg::CreateCampaign {
            denom: DENOM.to_string(),
            meta: String::new(),
            keeper,
            expiry,
            streaming: true,
            initial: None,
        },
    )
    .unwrap();
    from_json::<u64>(res.data.unwrap()).unwrap()
}

fn update_root(
    deps: &mut Deps,
    env: &Env,
    sender: &Addr,
    id: u64,
    root: &HexBinary,
    total: u128,
    attach: u128,
) -> Result<cosmwasm_std::Response, ContractError> {
    let funds = if attach == 0 {
        vec![]
    } else {
        coins(attach, DENOM)
    };
    execute(
        deps.as_mut(),
        env.clone(),
        message_info(sender, &funds),
        ExecuteMsg::UpdateRoot {
            id,
            root: root.clone(),
            total: Uint128::new(total),
            leaves_uri: "https://tools.trippyinj.xyz/leaves/1.json".to_string(),
        },
    )
}

fn claim(
    deps: &mut Deps,
    env: &Env,
    sender: &Addr,
    id: u64,
    amount: u128,
    proof: &[HexBinary],
) -> Result<cosmwasm_std::Response, ContractError> {
    execute(
        deps.as_mut(),
        env.clone(),
        message_info(sender, &[]),
        ExecuteMsg::Claim {
            id,
            amount: Uint128::new(amount),
            proof: proof.to_vec(),
        },
    )
}

fn query_campaign(deps: &Deps, env: &Env, id: u64) -> CampaignResponse {
    from_json(query(deps.as_ref(), env.clone(), QueryMsg::Campaign { id }).unwrap()).unwrap()
}

fn liabilities(deps: &Deps, env: &Env, denom: &str) -> Uint128 {
    let r: LiabilitiesResponse = from_json(
        query(
            deps.as_ref(),
            env.clone(),
            QueryMsg::Liabilities {
                denom: denom.to_string(),
            },
        )
        .unwrap(),
    )
    .unwrap();
    r.owed
}

fn assert_bank_send(msg: &SubMsg, to: &Addr, amount: u128, denom: &str) {
    match &msg.msg {
        CosmosMsg::Bank(BankMsg::Send {
            to_address,
            amount: sent,
        }) => {
            assert_eq!(to_address, to.as_str());
            assert_eq!(sent, &coins(amount, denom));
        }
        other => panic!("expected BankMsg::Send, got {other:?}"),
    }
}

// ---- tests ----

#[test]
fn instantiate_caps_fee_bps() {
    let mut deps = mock_dependencies();
    let owner = deps.api.addr_make("owner");
    let err = instantiate(
        deps.as_mut(),
        mock_env(),
        message_info(&owner, &[]),
        InstantiateMsg {
            owner: None,
            fee_bps: 1_001,
            fee_collector: None,
        },
    )
    .unwrap_err();
    assert_eq!(err, ContractError::FeeTooHigh { max: 1_000 });
}

#[test]
fn permission_split_streaming_is_owner_only() {
    let mut deps = mock_dependencies();
    let (env, _owner) = init(&mut deps, 0, None);
    let rando = deps.api.addr_make("rando");

    // non-owner cannot create a streaming campaign
    let err = execute(
        deps.as_mut(),
        env.clone(),
        message_info(&rando, &[]),
        ExecuteMsg::CreateCampaign {
            denom: DENOM.into(),
            meta: String::new(),
            keeper: None,
            expiry: None,
            streaming: true,
            initial: None,
        },
    )
    .unwrap_err();
    assert_eq!(err, ContractError::StreamingRequiresOwner);

    // non-owner cannot attach a keeper (implies streaming)
    let err = execute(
        deps.as_mut(),
        env.clone(),
        message_info(&rando, &[]),
        ExecuteMsg::CreateCampaign {
            denom: DENOM.into(),
            meta: String::new(),
            keeper: Some(rando.to_string()),
            expiry: None,
            streaming: false,
            initial: None,
        },
    )
    .unwrap_err();
    assert_eq!(err, ContractError::KeeperRequiresStreaming);
}

#[test]
fn one_shot_auto_freezes_on_first_publish() {
    let mut deps = mock_dependencies();
    let (env, _owner) = init(&mut deps, 0, None);
    let creator = deps.api.addr_make("creator");
    let alice = deps.api.addr_make("alice");
    let id = create_one_shot(&mut deps, &env, &creator, None);
    let (root, proofs) = build_tree(&[(alice.clone(), 100)]);

    update_root(&mut deps, &env, &creator, id, &root, 100, 100).unwrap();
    // frozen automatically: a second publish is rejected
    assert!(query_campaign(&deps, &env, id).campaign.frozen);
    let err = update_root(&mut deps, &env, &creator, id, &root, 200, 100).unwrap_err();
    assert_eq!(err, ContractError::Frozen);
    // but it stays claimable
    claim(&mut deps, &env, &alice, id, 100, &proofs[0]).unwrap();
}

#[test]
fn atomic_create_fund_and_freeze_one_shot() {
    let mut deps = mock_dependencies();
    let (env, _owner) = init(&mut deps, 0, None);
    let creator = deps.api.addr_make("creator");
    let alice = deps.api.addr_make("alice");
    let bob = deps.api.addr_make("bob");
    let (root, proofs) = build_tree(&[(alice.clone(), 700), (bob.clone(), 300)]);

    // wrong attached amount is rejected atomically
    let err = execute(
        deps.as_mut(),
        env.clone(),
        message_info(&creator, &coins(999, DENOM)),
        ExecuteMsg::CreateCampaign {
            denom: DENOM.into(),
            meta: String::new(),
            keeper: None,
            expiry: None,
            streaming: false,
            initial: Some(InitialRoot {
                root: root.clone(),
                total: Uint128::new(1_000),
                leaves_uri: String::new(),
            }),
        },
    )
    .unwrap_err();
    assert!(matches!(err, ContractError::InvalidFunds { .. }));

    // correct: create + fund + publish + auto-freeze in one tx
    let res = execute(
        deps.as_mut(),
        env.clone(),
        message_info(&creator, &coins(1_000, DENOM)),
        ExecuteMsg::CreateCampaign {
            denom: DENOM.into(),
            meta: String::new(),
            keeper: None,
            expiry: None,
            streaming: false,
            initial: Some(InitialRoot {
                root: root.clone(),
                total: Uint128::new(1_000),
                leaves_uri: String::new(),
            }),
        },
    )
    .unwrap();
    let id: u64 = from_json(res.data.unwrap()).unwrap();
    let c = query_campaign(&deps, &env, id);
    assert!(c.campaign.frozen);
    assert_eq!(c.remaining, Uint128::new(1_000));
    assert_eq!(liabilities(&deps, &env, DENOM), Uint128::new(1_000));

    claim(&mut deps, &env, &alice, id, 700, &proofs[0]).unwrap();
    claim(&mut deps, &env, &bob, id, 300, &proofs[1]).unwrap();
    assert_eq!(query_campaign(&deps, &env, id).remaining, Uint128::zero());
    assert_eq!(liabilities(&deps, &env, DENOM), Uint128::zero());
}

#[test]
fn streaming_allows_repeated_updates_and_keeper() {
    let mut deps = mock_dependencies();
    let (env, owner) = init(&mut deps, 0, None);
    let keeper = deps.api.addr_make("keeper");
    let alice = deps.api.addr_make("alice");
    let id = create_streaming(&mut deps, &env, &owner, Some(keeper.to_string()), None);

    let (root1, proofs1) = build_tree(&[(alice.clone(), 100)]);
    update_root(&mut deps, &env, &keeper, id, &root1, 100, 100).unwrap();
    assert!(!query_campaign(&deps, &env, id).campaign.frozen);
    claim(&mut deps, &env, &alice, id, 100, &proofs1[0]).unwrap();

    // next epoch: keeper raises the lifetime total, attaches the delta only
    let (root2, proofs2) = build_tree(&[(alice.clone(), 250)]);
    update_root(&mut deps, &env, &keeper, id, &root2, 250, 150).unwrap();
    let res = claim(&mut deps, &env, &alice, id, 250, &proofs2[0]).unwrap();
    assert_bank_send(&res.messages[0], &alice, 150, DENOM);
}

#[test]
fn keeper_zero_delta_republish_still_bounded_but_note_streaming_trust() {
    // Documents the streaming trust model: a keeper CAN reassign unclaimed float
    // on a streaming campaign (owner-only to create). This is why streaming is
    // owner-gated; one-shots (auto-frozen) are immune to the same move.
    let mut deps = mock_dependencies();
    let (env, owner) = init(&mut deps, 0, None);
    let keeper = deps.api.addr_make("keeper");
    let alice = deps.api.addr_make("alice");
    let id = create_streaming(&mut deps, &env, &owner, Some(keeper.to_string()), None);
    let (root, _) = build_tree(&[(alice.clone(), 1_000)]);
    update_root(&mut deps, &env, &keeper, id, &root, 1_000, 1_000).unwrap();

    // one-shot equivalent would be frozen; confirm the frozen guard is the wall
    let creator = deps.api.addr_make("creator");
    let one = create_one_shot(&mut deps, &env, &creator, None);
    let (r2, _) = build_tree(&[(alice.clone(), 500)]);
    update_root(&mut deps, &env, &creator, one, &r2, 500, 500).unwrap();
    let (evil, _) = build_tree(&[(creator.clone(), 500)]);
    let err = update_root(&mut deps, &env, &creator, one, &evil, 500, 0).unwrap_err();
    assert_eq!(err, ContractError::Frozen);
}

#[test]
fn update_root_enforces_exact_funding() {
    let mut deps = mock_dependencies();
    let (env, owner) = init(&mut deps, 0, None);
    let alice = deps.api.addr_make("alice");
    let id = create_streaming(&mut deps, &env, &owner, None, None);
    let (root, _) = build_tree(&[(alice, 100)]);

    for attach in [99u128, 101] {
        let err = update_root(&mut deps, &env, &owner, id, &root, 100, attach).unwrap_err();
        assert!(matches!(err, ContractError::InvalidFunds { .. }));
    }
    let err = execute(
        deps.as_mut(),
        env.clone(),
        message_info(&owner, &coins(100, "inj")),
        ExecuteMsg::UpdateRoot {
            id,
            root: root.clone(),
            total: Uint128::new(100),
            leaves_uri: String::new(),
        },
    )
    .unwrap_err();
    assert!(matches!(err, ContractError::InvalidFunds { .. }));

    update_root(&mut deps, &env, &owner, id, &root, 100, 100).unwrap();
    assert_eq!(query_campaign(&deps, &env, id).remaining, Uint128::new(100));

    // zero-delta republish (corrected tree, same total): no funds allowed
    update_root(&mut deps, &env, &owner, id, &root, 100, 0).unwrap();
    let err = update_root(&mut deps, &env, &owner, id, &root, 100, 1).unwrap_err();
    assert_eq!(err, ContractError::UnexpectedFunds);
}

#[test]
fn fee_is_ceiled_and_charged_on_top() {
    let mut deps = mock_dependencies();
    let collector = deps.api.addr_make("collector");
    let (env, _owner) = init(&mut deps, 100, Some(collector.to_string())); // 1%
    let owner = deps.api.addr_make("owner");
    let alice = deps.api.addr_make("alice");
    let id = create_streaming(&mut deps, &env, &owner, None, None);

    // delta 101 at 1% -> ceil(1.01) = 2, so attach 103; collector gets 2
    let (root, proofs) = build_tree(&[(alice.clone(), 101)]);
    let err = update_root(&mut deps, &env, &owner, id, &root, 101, 102).unwrap_err();
    assert!(matches!(err, ContractError::InvalidFunds { .. }));
    let res = update_root(&mut deps, &env, &owner, id, &root, 101, 103).unwrap();
    assert_eq!(res.messages.len(), 1);
    assert_bank_send(&res.messages[0], &collector, 2, DENOM);

    // campaign keeps exactly the declared total for claimants
    assert_eq!(liabilities(&deps, &env, DENOM), Uint128::new(101));
    let res = claim(&mut deps, &env, &alice, id, 101, &proofs[0]).unwrap();
    assert_bank_send(&res.messages[0], &alice, 101, DENOM);
}

#[test]
fn funding_required_query_matches_exact_funds() {
    let mut deps = mock_dependencies();
    let (env, _owner) = init(&mut deps, 100, None);
    let owner = deps.api.addr_make("owner");
    let id = create_streaming(&mut deps, &env, &owner, None, None);
    let alice = deps.api.addr_make("alice");
    let (root, _) = build_tree(&[(alice, 101)]);

    let r: FundingRequiredResponse = from_json(
        query(
            deps.as_ref(),
            env.clone(),
            QueryMsg::FundingRequired {
                id,
                new_total: Uint128::new(101),
            },
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(r.delta, Uint128::new(101));
    assert_eq!(r.fee, Uint128::new(2));
    assert_eq!(r.required, Uint128::new(103));
    assert_eq!(r.denom, DENOM);
    // attaching exactly `required` succeeds
    update_root(&mut deps, &env, &owner, id, &root, 101, r.required.u128()).unwrap();
}

#[test]
fn claim_pays_once_and_caps_overallocation_per_campaign() {
    let mut deps = mock_dependencies();
    let (env, owner) = init(&mut deps, 0, None);
    let alice = deps.api.addr_make("alice");
    let bob = deps.api.addr_make("bob");
    let carol = deps.api.addr_make("carol");

    // dishonest tree: leaves sum 200, only 100 declared/attached
    let id = create_streaming(&mut deps, &env, &owner, None, None);
    let (root, proofs) = build_tree(&[(alice.clone(), 100), (bob.clone(), 100)]);
    update_root(&mut deps, &env, &owner, id, &root, 100, 100).unwrap();

    // isolated honest campaign
    let id2 = create_streaming(&mut deps, &env, &owner, None, None);
    let (root2, proofs2) = build_tree(&[(carol.clone(), 500)]);
    update_root(&mut deps, &env, &owner, id2, &root2, 500, 500).unwrap();

    claim(&mut deps, &env, &alice, id, 100, &proofs[0]).unwrap();
    let err = claim(&mut deps, &env, &alice, id, 100, &proofs[0]).unwrap_err();
    assert_eq!(err, ContractError::NothingToClaim);
    let err = claim(&mut deps, &env, &bob, id, 100, &proofs[1]).unwrap_err();
    assert_eq!(err, ContractError::ExceedsCampaignFunds);

    assert_eq!(
        query_campaign(&deps, &env, id2).remaining,
        Uint128::new(500)
    );
    let res = claim(&mut deps, &env, &carol, id2, 500, &proofs2[0]).unwrap();
    assert_bank_send(&res.messages[0], &carol, 500, DENOM);
}

#[test]
fn previous_root_stays_claimable_after_update() {
    let mut deps = mock_dependencies();
    let (env, owner) = init(&mut deps, 0, None);
    let alice = deps.api.addr_make("alice");
    let bob = deps.api.addr_make("bob");
    let id = create_streaming(&mut deps, &env, &owner, None, None);

    let (root1, proofs1) = build_tree(&[(alice.clone(), 100), (bob.clone(), 50)]);
    update_root(&mut deps, &env, &owner, id, &root1, 150, 150).unwrap();
    let (root2, proofs2) = build_tree(&[(alice.clone(), 250), (bob.clone(), 50)]);
    update_root(&mut deps, &env, &owner, id, &root2, 300, 150).unwrap();

    // bob's pre-update proof still verifies via prev_root
    let res = claim(&mut deps, &env, &bob, id, 50, &proofs1[1]).unwrap();
    assert_bank_send(&res.messages[0], &bob, 50, DENOM);
    let err = claim(&mut deps, &env, &bob, id, 50, &proofs2[1]).unwrap_err();
    assert_eq!(err, ContractError::NothingToClaim);
}

#[test]
fn invalid_proofs_are_rejected() {
    let mut deps = mock_dependencies();
    let (env, owner) = init(&mut deps, 0, None);
    let alice = deps.api.addr_make("alice");
    let bob = deps.api.addr_make("bob");
    let id = create_streaming(&mut deps, &env, &owner, None, None);
    let (root, proofs) = build_tree(&[(alice.clone(), 100), (bob.clone(), 50)]);
    update_root(&mut deps, &env, &owner, id, &root, 150, 150).unwrap();

    let err = claim(&mut deps, &env, &alice, id, 101, &proofs[0]).unwrap_err();
    assert_eq!(err, ContractError::InvalidProof);
    let err = claim(&mut deps, &env, &alice, id, 50, &proofs[1]).unwrap_err();
    assert_eq!(err, ContractError::InvalidProof);
}

#[test]
fn freeze_locks_expiry_of_a_perpetual_drop() {
    let mut deps = mock_dependencies();
    let (env, _owner) = init(&mut deps, 0, None);
    let creator = deps.api.addr_make("creator");
    let alice = deps.api.addr_make("alice");
    // perpetual one-shot: create+publish auto-freezes, expiry stays None
    let id = create_one_shot(&mut deps, &env, &creator, None);
    let (root, _) = build_tree(&[(alice.clone(), 100)]);
    update_root(&mut deps, &env, &creator, id, &root, 100, 100).unwrap();
    assert!(query_campaign(&deps, &env, id).campaign.frozen);

    // cannot introduce an expiry (and therefore cannot ever claw back)
    let err = execute(
        deps.as_mut(),
        env.clone(),
        message_info(&creator, &[]),
        ExecuteMsg::SetExpiry {
            id,
            expiry: Timestamp::from_seconds(T0 + 30 * 24 * 3_600),
        },
    )
    .unwrap_err();
    assert_eq!(err, ContractError::FrozenExpiryLocked);
}

#[test]
fn expiry_blocks_claims_and_enables_clawback() {
    let mut deps = mock_dependencies();
    let (env, _owner) = init(&mut deps, 0, None);
    let creator = deps.api.addr_make("creator");
    let alice = deps.api.addr_make("alice");
    let bob = deps.api.addr_make("bob");
    let expiry = Timestamp::from_seconds(T0 + 1_000);
    let id = create_one_shot(&mut deps, &env, &creator, Some(expiry));
    let (root, proofs) = build_tree(&[(alice.clone(), 100), (bob.clone(), 50)]);
    update_root(&mut deps, &env, &creator, id, &root, 150, 150).unwrap();

    claim(&mut deps, &env, &alice, id, 100, &proofs[0]).unwrap();
    assert_eq!(liabilities(&deps, &env, DENOM), Uint128::new(50));

    let err = execute(
        deps.as_mut(),
        env.clone(),
        message_info(&creator, &[]),
        ExecuteMsg::Clawback { id },
    )
    .unwrap_err();
    assert_eq!(err, ContractError::NotExpired);

    let mut late = env.clone();
    late.block.time = expiry;
    let err = claim(&mut deps, &late, &bob, id, 50, &proofs[1]).unwrap_err();
    assert_eq!(err, ContractError::Expired);

    // bob's unclaimed 50 is swept to the creator, and liabilities clear
    let res = execute(
        deps.as_mut(),
        late.clone(),
        message_info(&creator, &[]),
        ExecuteMsg::Clawback { id },
    )
    .unwrap();
    assert_bank_send(&res.messages[0], &creator, 50, DENOM);
    assert_eq!(liabilities(&deps, &late, DENOM), Uint128::zero());
    // swept campaign reports zero remaining
    assert_eq!(query_campaign(&deps, &late, id).remaining, Uint128::zero());

    let err = execute(
        deps.as_mut(),
        late.clone(),
        message_info(&creator, &[]),
        ExecuteMsg::Clawback { id },
    )
    .unwrap_err();
    assert_eq!(err, ContractError::Swept);
}

#[test]
fn two_step_ownership_transfer() {
    let mut deps = mock_dependencies();
    let (env, owner) = init(&mut deps, 0, None);
    let new_owner = deps.api.addr_make("new_owner");
    let rando = deps.api.addr_make("rando");

    execute(
        deps.as_mut(),
        env.clone(),
        message_info(&owner, &[]),
        ExecuteMsg::TransferOwnership {
            new_owner: new_owner.to_string(),
        },
    )
    .unwrap();

    // old owner still in charge until accepted
    let err = execute(
        deps.as_mut(),
        env.clone(),
        message_info(&rando, &[]),
        ExecuteMsg::AcceptOwnership {},
    )
    .unwrap_err();
    assert_eq!(err, ContractError::NoPendingOwnership);

    execute(
        deps.as_mut(),
        env.clone(),
        message_info(&new_owner, &[]),
        ExecuteMsg::AcceptOwnership {},
    )
    .unwrap();

    let cfg: Config =
        from_json(query(deps.as_ref(), env.clone(), QueryMsg::Config {}).unwrap()).unwrap();
    assert_eq!(cfg.owner, new_owner);
    assert_eq!(cfg.pending_owner, None);

    // old owner can no longer create streaming campaigns; new owner can
    let err = execute(
        deps.as_mut(),
        env.clone(),
        message_info(&owner, &[]),
        ExecuteMsg::CreateCampaign {
            denom: DENOM.into(),
            meta: String::new(),
            keeper: None,
            expiry: None,
            streaming: true,
            initial: None,
        },
    )
    .unwrap_err();
    assert_eq!(err, ContractError::StreamingRequiresOwner);
    create_streaming(&mut deps, &env, &new_owner, None, None);
}

#[test]
fn two_step_creator_transfer_moves_index() {
    let mut deps = mock_dependencies();
    let (env, _owner) = init(&mut deps, 0, None);
    let creator = deps.api.addr_make("creator");
    let new_creator = deps.api.addr_make("new_creator");
    let id = create_one_shot(&mut deps, &env, &creator, None);

    execute(
        deps.as_mut(),
        env.clone(),
        message_info(&creator, &[]),
        ExecuteMsg::TransferCreator {
            id,
            new_creator: new_creator.to_string(),
        },
    )
    .unwrap();
    // wrong acceptor
    let err = execute(
        deps.as_mut(),
        env.clone(),
        message_info(&creator, &[]),
        ExecuteMsg::AcceptCreator { id },
    )
    .unwrap_err();
    assert_eq!(err, ContractError::NoPendingCreator);

    execute(
        deps.as_mut(),
        env.clone(),
        message_info(&new_creator, &[]),
        ExecuteMsg::AcceptCreator { id },
    )
    .unwrap();

    // creator power moved: old creator can't freeze, new one can administer
    let err = execute(
        deps.as_mut(),
        env.clone(),
        message_info(&creator, &[]),
        ExecuteMsg::SetCampaignPaused { id, paused: true },
    )
    .unwrap_err();
    assert_eq!(err, ContractError::Unauthorized);

    // index moved to new creator
    let by_new: Vec<CampaignResponse> = from_json(
        query(
            deps.as_ref(),
            env.clone(),
            QueryMsg::CampaignsByCreator {
                creator: new_creator.to_string(),
                start_after: None,
                limit: None,
            },
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(by_new.len(), 1);
    let by_old: Vec<CampaignResponse> = from_json(
        query(
            deps.as_ref(),
            env.clone(),
            QueryMsg::CampaignsByCreator {
                creator: creator.to_string(),
                start_after: None,
                limit: None,
            },
        )
        .unwrap(),
    )
    .unwrap();
    assert!(by_old.is_empty());
}

#[test]
fn claim_many_atomic_and_partial() {
    let mut deps = mock_dependencies();
    let (env, owner) = init(&mut deps, 0, None);
    let alice = deps.api.addr_make("alice");
    let id1 = create_streaming(&mut deps, &env, &owner, None, None);
    let (root1, proofs1) = build_tree(&[(alice.clone(), 100)]);
    update_root(&mut deps, &env, &owner, id1, &root1, 100, 100).unwrap();
    let id2 = create_streaming(&mut deps, &env, &owner, None, None);
    let (root2, proofs2) = build_tree(&[(alice.clone(), 40)]);
    update_root(&mut deps, &env, &owner, id2, &root2, 40, 40).unwrap();

    // pause id2 so a naive claim-all would fail
    execute(
        deps.as_mut(),
        env.clone(),
        message_info(&owner, &[]),
        ExecuteMsg::SetCampaignPaused {
            id: id2,
            paused: true,
        },
    )
    .unwrap();

    let alice_id1 = ClaimMsg {
        id: id1,
        amount: Uint128::new(100),
        proof: proofs1[0].clone(),
    };
    let alice_id2 = ClaimMsg {
        id: id2,
        amount: Uint128::new(40),
        proof: proofs2[0].clone(),
    };

    // atomic: the whole thing reverts on the paused campaign. Paused campaign is
    // listed first so the error fires before any state is written (unit-test
    // execute() does not roll back storage on Err the way the chain does).
    let err = execute(
        deps.as_mut(),
        env.clone(),
        message_info(&alice, &[]),
        ExecuteMsg::ClaimMany {
            claims: vec![alice_id2.clone(), alice_id1.clone()],
            allow_partial: None,
        },
    )
    .unwrap_err();
    assert_eq!(err, ContractError::CampaignPaused);

    // partial: id1 pays, id2 is skipped
    let res = execute(
        deps.as_mut(),
        env.clone(),
        message_info(&alice, &[]),
        ExecuteMsg::ClaimMany {
            claims: vec![alice_id1, alice_id2],
            allow_partial: Some(true),
        },
    )
    .unwrap();
    assert_eq!(res.messages.len(), 1);
    assert_bank_send(&res.messages[0], &alice, 100, DENOM);
}

#[test]
fn rescue_only_sweeps_unencumbered_balance() {
    // Seed the contract with 1_100 of DENOM; a campaign will owe 1_000.
    let mut deps = mock_dependencies_with_balance(&[Coin::new(1_100u128, DENOM)]);
    let (env, owner) = init(&mut deps, 0, None);
    let creator = deps.api.addr_make("creator");
    let alice = deps.api.addr_make("alice");
    let (root, _) = build_tree(&[(alice.clone(), 1_000)]);
    execute(
        deps.as_mut(),
        env.clone(),
        message_info(&creator, &coins(1_000, DENOM)),
        ExecuteMsg::CreateCampaign {
            denom: DENOM.into(),
            meta: String::new(),
            keeper: None,
            expiry: None,
            streaming: false,
            initial: Some(InitialRoot {
                root,
                total: Uint128::new(1_000),
                leaves_uri: String::new(),
            }),
        },
    )
    .unwrap();

    // non-owner cannot rescue
    let err = execute(
        deps.as_mut(),
        env.clone(),
        message_info(&creator, &[]),
        ExecuteMsg::Rescue {
            denom: DENOM.into(),
            amount: None,
            recipient: None,
        },
    )
    .unwrap_err();
    assert_eq!(err, ContractError::Unauthorized);

    // asking for more than the 100 excess is rejected
    let err = execute(
        deps.as_mut(),
        env.clone(),
        message_info(&owner, &[]),
        ExecuteMsg::Rescue {
            denom: DENOM.into(),
            amount: Some(Uint128::new(101)),
            recipient: None,
        },
    )
    .unwrap_err();
    assert_eq!(
        err,
        ContractError::RescueExceedsExcess {
            available: Uint128::new(100)
        }
    );

    // sweeping the excess (100) to owner succeeds; claimant funds untouched
    let res = execute(
        deps.as_mut(),
        env.clone(),
        message_info(&owner, &[]),
        ExecuteMsg::Rescue {
            denom: DENOM.into(),
            amount: None,
            recipient: None,
        },
    )
    .unwrap();
    assert_bank_send(&res.messages[0], &owner, 100, DENOM);
}

#[test]
fn pause_paths() {
    let mut deps = mock_dependencies();
    let (env, owner) = init(&mut deps, 0, None);
    let alice = deps.api.addr_make("alice");
    let id = create_streaming(&mut deps, &env, &owner, None, None);
    let (root, proofs) = build_tree(&[(alice.clone(), 100)]);
    update_root(&mut deps, &env, &owner, id, &root, 100, 100).unwrap();

    // global pause blocks funds-moving messages
    execute(
        deps.as_mut(),
        env.clone(),
        message_info(&owner, &[]),
        ExecuteMsg::UpdateConfig {
            fee_bps: None,
            fee_collector: None,
            paused: Some(true),
        },
    )
    .unwrap();
    let err = claim(&mut deps, &env, &alice, id, 100, &proofs[0]).unwrap_err();
    assert_eq!(err, ContractError::Paused);
    execute(
        deps.as_mut(),
        env.clone(),
        message_info(&owner, &[]),
        ExecuteMsg::UpdateConfig {
            fee_bps: None,
            fee_collector: None,
            paused: Some(false),
        },
    )
    .unwrap();

    // campaign pause blocks claims but not administration
    execute(
        deps.as_mut(),
        env.clone(),
        message_info(&owner, &[]),
        ExecuteMsg::SetCampaignPaused { id, paused: true },
    )
    .unwrap();
    let err = claim(&mut deps, &env, &alice, id, 100, &proofs[0]).unwrap_err();
    assert_eq!(err, ContractError::CampaignPaused);
    execute(
        deps.as_mut(),
        env.clone(),
        message_info(&owner, &[]),
        ExecuteMsg::SetCampaignPaused { id, paused: false },
    )
    .unwrap();
    claim(&mut deps, &env, &alice, id, 100, &proofs[0]).unwrap();
}

#[test]
fn claims_and_claimable_queries() {
    let mut deps = mock_dependencies();
    let (env, owner) = init(&mut deps, 0, None);
    let alice = deps.api.addr_make("alice");
    let bob = deps.api.addr_make("bob");
    let id = create_streaming(&mut deps, &env, &owner, None, None);
    let (root, proofs) = build_tree(&[(alice.clone(), 100), (bob.clone(), 50)]);
    update_root(&mut deps, &env, &owner, id, &root, 150, 150).unwrap();

    // Claimable dry-run
    let r: ClaimableResponse = from_json(
        query(
            deps.as_ref(),
            env.clone(),
            QueryMsg::Claimable {
                id,
                address: alice.to_string(),
                amount: Uint128::new(100),
                proof: proofs[0].clone(),
            },
        )
        .unwrap(),
    )
    .unwrap();
    assert!(r.valid);
    assert_eq!(r.payable, Uint128::new(100));

    claim(&mut deps, &env, &alice, id, 100, &proofs[0]).unwrap();
    claim(&mut deps, &env, &bob, id, 50, &proofs[1]).unwrap();

    // claimants counter + paginated Claims list
    assert_eq!(query_campaign(&deps, &env, id).campaign.claimants, 2);
    let page: ClaimsResponse = from_json(
        query(
            deps.as_ref(),
            env.clone(),
            QueryMsg::Claims {
                id,
                start_after: None,
                limit: Some(10),
            },
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(page.claims.len(), 2);
    let total: u128 = page
        .claims
        .iter()
        .map(|c: &ClaimEntry| c.claimed.u128())
        .sum();
    assert_eq!(total, 150);
}

#[test]
fn migrate_rejects_downgrade() {
    let mut deps = mock_dependencies();
    let (_env, _owner) = init(&mut deps, 0, None);
    // pretend a newer version is stored, then migrate with the (older) crate version
    cw2::set_contract_version(&mut deps.storage, "crates.io:choice-claim-drops", "9.9.9").unwrap();
    let err = migrate(deps.as_mut(), mock_env(), MigrateMsg {}).unwrap_err();
    match err {
        ContractError::Std(e) => assert!(e.to_string().contains("cannot migrate down")),
        other => panic!("expected downgrade rejection, got {other:?}"),
    }
}
