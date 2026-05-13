use cosmwasm_std::testing::{message_info, mock_env, MockApi, MockQuerier, MockStorage};
use cosmwasm_std::{coin, coins, Addr, Empty, OwnedDeps, Uint128};
use std::marker::PhantomData;

use injective_cosmwasm::query::InjectiveQueryWrapper;

use crate::contract::{execute, instantiate};
use crate::error::ContractError;
use crate::math::{isqrt, optimal_swap_in, simulate_swap_return};
use crate::msg::{CallbackMsg, ExecuteMsg, InstantiateMsg};

/// Minimal mock deps for paths that only exercise auth + funds parsing — they
/// never reach a wasm/bank query, so the simple `MockQuerier` is enough.
fn mock_deps_simple(
) -> OwnedDeps<MockStorage, MockApi, MockQuerier<Empty>, InjectiveQueryWrapper> {
    OwnedDeps {
        storage: MockStorage::default(),
        api: MockApi::default(),
        querier: MockQuerier::<Empty>::new(&[]),
        custom_query_type: PhantomData,
    }
}

fn owner_addr(api: &MockApi) -> Addr {
    api.addr_make("owner")
}

fn do_instantiate(
    deps: &mut OwnedDeps<MockStorage, MockApi, MockQuerier<Empty>, InjectiveQueryWrapper>,
) {
    let owner = owner_addr(&deps.api);
    let info = message_info(&owner, &[]);
    instantiate(
        deps.as_mut(),
        mock_env(),
        info,
        InstantiateMsg {
            owner: None,
            default_recipient: None,
            tip_bps: None,
            min_zap_amount: None,
        },
    )
    .unwrap();
}

fn do_instantiate_with_defaults(
    deps: &mut OwnedDeps<MockStorage, MockApi, MockQuerier<Empty>, InjectiveQueryWrapper>,
    default_recipient: Option<String>,
    tip_bps: Option<u16>,
    min_zap_amount: Option<Uint128>,
) {
    let owner = owner_addr(&deps.api);
    let info = message_info(&owner, &[]);
    instantiate(
        deps.as_mut(),
        mock_env(),
        info,
        InstantiateMsg {
            owner: None,
            default_recipient,
            tip_bps,
            min_zap_amount,
        },
    )
    .unwrap();
}

// -------- math --------

#[test]
fn isqrt_basic() {
    use cosmwasm_std::Uint256;
    assert_eq!(isqrt(Uint256::from(0u128)).unwrap(), Uint256::from(0u128));
    assert_eq!(isqrt(Uint256::from(1u128)).unwrap(), Uint256::from(1u128));
    assert_eq!(isqrt(Uint256::from(2u128)).unwrap(), Uint256::from(1u128));
    assert_eq!(isqrt(Uint256::from(3u128)).unwrap(), Uint256::from(1u128));
    assert_eq!(isqrt(Uint256::from(4u128)).unwrap(), Uint256::from(2u128));
    assert_eq!(isqrt(Uint256::from(99u128)).unwrap(), Uint256::from(9u128));
    assert_eq!(isqrt(Uint256::from(100u128)).unwrap(), Uint256::from(10u128));
    let big = Uint256::from(10u128).pow(40);
    let expected = Uint256::from(10u128).pow(20);
    assert_eq!(isqrt(big).unwrap(), expected);
}

#[test]
fn optimal_split_zero_input() {
    assert_eq!(
        optimal_swap_in(Uint128::new(1_000_000), Uint128::zero()).unwrap(),
        Uint128::zero()
    );
}

#[test]
fn optimal_split_empty_pool() {
    assert!(matches!(
        optimal_swap_in(Uint128::zero(), Uint128::new(1)),
        Err(ContractError::EmptyPool {})
    ));
}

/// For a deep, balanced pool the optimal swap is just *above* half — you have
/// to sell a hair more A so that the fee-haircut B you receive matches the
/// post-swap pool ratio of the remaining A. Asymptotically s/X → 1/(2-f),
/// which is 1/1.997 ≈ 0.5008 for the pair's 0.3% fee.
#[test]
fn optimal_split_deep_pool_is_just_over_half() {
    // Pool: 1e18 raw / side. Input 1e12 (a tiny fraction of reserves).
    let r_a = Uint128::new(1_000_000_000_000_000_000);
    let x = Uint128::new(1_000_000_000_000);
    let s = optimal_swap_in(r_a, x).unwrap();
    let half = x.u128() / 2;
    assert!(s.u128() >= half, "s {} should be >= half {}", s, half);
    // Within 0.5% over half — matches the 0.3% fee + price impact band.
    let upper = half + half / 200;
    assert!(s.u128() <= upper, "s {} should be <= {} (half+0.5%)", s, upper);
}

/// Sanity: post-swap, (X-s)/r_a_post should match b/r_b_post within a few wei.
#[test]
fn optimal_split_balances_post_swap_ratio() {
    let r_a = Uint128::new(5_000_000_000_000);
    let r_b = Uint128::new(2_000_000_000_000);
    let x = Uint128::new(100_000_000);

    let s = optimal_swap_in(r_a, x).unwrap();
    let b = simulate_swap_return(r_a, r_b, s).unwrap();
    let remaining = x - s;

    // Post-swap pool reserves (commission stays in pool; ignore burn/fee_wallet
    // slivers for this ratio check — they're 1/6 each of a 0.3% cut).
    let r_a_post = r_a + s;
    let r_b_post = r_b - b;

    // remaining / r_a_post vs b / r_b_post — cross-multiply.
    let lhs = remaining.u128() * r_b_post.u128();
    let rhs = b.u128() * r_a_post.u128();
    let diff = lhs.abs_diff(rhs);
    // Expect tight match; tolerance covers integer rounding + ignoring the
    // 0.05%/0.05% fee-wallet & burn outflows from r_b_post.
    let tolerance = lhs / 100; // 1%
    assert!(
        diff <= tolerance,
        "ratios diverged: remaining/{} vs b/{} (lhs={}, rhs={}, diff={})",
        r_a_post,
        r_b_post,
        lhs,
        rhs,
        diff
    );
}

// -------- execute: input validation --------

// `Zap` is permissionless: any user can call it with their own funds. The
// snapshot logic in `execute_zap` keeps each call isolated to its own funds —
// pre-existing balances are not reachable by the caller's recipient.

#[test]
fn zap_rejects_zero_funds() {
    let mut deps = mock_deps_simple();
    do_instantiate(&mut deps);
    let caller = deps.api.addr_make("caller");
    let pair = deps.api.addr_make("pair").to_string();

    let err = execute(
        deps.as_mut(),
        mock_env(),
        message_info(&caller, &[]),
        ExecuteMsg::Zap {
            pair,
            recipient: None,
            max_spread: None,
            slippage_tolerance: None,
            min_lp_out: None,
            deadline: None,
        },
    )
    .unwrap_err();
    assert!(matches!(err, ContractError::InvalidInputFunds { count: 0 }));
}

#[test]
fn zap_rejects_multiple_funds() {
    let mut deps = mock_deps_simple();
    do_instantiate(&mut deps);
    let caller = deps.api.addr_make("caller");
    let pair = deps.api.addr_make("pair").to_string();

    let err = execute(
        deps.as_mut(),
        mock_env(),
        message_info(&caller, &[coin(100, "uatom"), coin(50, "inj")]),
        ExecuteMsg::Zap {
            pair,
            recipient: None,
            max_spread: None,
            slippage_tolerance: None,
            min_lp_out: None,
            deadline: None,
        },
    )
    .unwrap_err();
    assert!(matches!(err, ContractError::InvalidInputFunds { count: 2 }));
}

#[test]
fn zap_rejects_zero_amount() {
    let mut deps = mock_deps_simple();
    do_instantiate(&mut deps);
    let caller = deps.api.addr_make("caller");
    let pair = deps.api.addr_make("pair").to_string();

    let err = execute(
        deps.as_mut(),
        mock_env(),
        message_info(&caller, &[coin(0, "inj")]),
        ExecuteMsg::Zap {
            pair,
            recipient: None,
            max_spread: None,
            slippage_tolerance: None,
            min_lp_out: None,
            deadline: None,
        },
    )
    .unwrap_err();
    assert!(matches!(err, ContractError::ZeroInputAmount {}));
}

#[test]
fn zap_rejects_expired_deadline() {
    let mut deps = mock_deps_simple();
    do_instantiate(&mut deps);
    let caller = deps.api.addr_make("caller");
    let pair = deps.api.addr_make("pair").to_string();
    let env = mock_env();
    let past = env.block.time.seconds();

    let err = execute(
        deps.as_mut(),
        env,
        message_info(&caller, &coins(1_000, "inj")),
        ExecuteMsg::Zap {
            pair,
            recipient: None,
            max_spread: None,
            slippage_tolerance: None,
            min_lp_out: None,
            deadline: Some(past),
        },
    )
    .unwrap_err();
    assert!(matches!(err, ContractError::ExpiredDeadline {}));
}

// -------- ZapBalance --------

fn register_route(
    deps: &mut OwnedDeps<MockStorage, MockApi, MockQuerier<Empty>, InjectiveQueryWrapper>,
    input_denom: &str,
    pair: &str,
) {
    let owner = owner_addr(&deps.api);
    execute(
        deps.as_mut(),
        mock_env(),
        message_info(&owner, &[]),
        ExecuteMsg::RegisterRoute {
            input_denom: input_denom.to_string(),
            pair: pair.to_string(),
        },
    )
    .unwrap();
}

fn add_keeper(
    deps: &mut OwnedDeps<MockStorage, MockApi, MockQuerier<Empty>, InjectiveQueryWrapper>,
    address: &Addr,
) {
    let owner = owner_addr(&deps.api);
    execute(
        deps.as_mut(),
        mock_env(),
        message_info(&owner, &[]),
        ExecuteMsg::AddKeeper {
            address: address.to_string(),
        },
    )
    .unwrap();
}

#[test]
fn zap_balance_rejects_non_keeper() {
    let mut deps = mock_deps_simple();
    let treasury = deps.api.addr_make("treasury");
    do_instantiate_with_defaults(&mut deps, Some(treasury.to_string()), None, None);
    let stranger = deps.api.addr_make("stranger");

    let err = execute(
        deps.as_mut(),
        mock_env(),
        message_info(&stranger, &[]),
        ExecuteMsg::ZapBalance {
            input_denom: "inj".into(),
            max_spread: None,
            slippage_tolerance: None,
            min_lp_out: None,
            deadline: None,
        },
    )
    .unwrap_err();
    assert!(matches!(err, ContractError::NotKeeper {}));
}

#[test]
fn zap_balance_requires_default_recipient() {
    let mut deps = mock_deps_simple();
    do_instantiate(&mut deps); // no default_recipient set
    // Owner is implicitly allowed, so we expect to fail on default_recipient,
    // not auth.
    let owner = owner_addr(&deps.api);

    let err = execute(
        deps.as_mut(),
        mock_env(),
        message_info(&owner, &[]),
        ExecuteMsg::ZapBalance {
            input_denom: "inj".into(),
            max_spread: None,
            slippage_tolerance: None,
            min_lp_out: None,
            deadline: None,
        },
    )
    .unwrap_err();
    assert!(matches!(err, ContractError::DefaultRecipientUnset {}));
}

#[test]
fn zap_balance_requires_registered_route() {
    let mut deps = mock_deps_simple();
    let treasury = deps.api.addr_make("treasury");
    do_instantiate_with_defaults(&mut deps, Some(treasury.to_string()), None, None);
    let owner = owner_addr(&deps.api);

    let err = execute(
        deps.as_mut(),
        mock_env(),
        message_info(&owner, &[]),
        ExecuteMsg::ZapBalance {
            input_denom: "inj".into(),
            max_spread: None,
            slippage_tolerance: None,
            min_lp_out: None,
            deadline: None,
        },
    )
    .unwrap_err();
    assert!(matches!(err, ContractError::NoRouteForDenom { .. }));
}

#[test]
fn zap_balance_below_min_aborts() {
    let mut deps = mock_deps_simple();
    let treasury = deps.api.addr_make("treasury");
    do_instantiate_with_defaults(
        &mut deps,
        Some(treasury.to_string()),
        Some(0),
        Some(Uint128::new(1_000_000)),
    );
    let pair = deps.api.addr_make("pair").to_string();
    register_route(&mut deps, "inj", &pair);
    let owner = owner_addr(&deps.api);

    // MockQuerier returns 0 for unset balances — strictly below the 1_000_000
    // min, so we expect BalanceBelowMin before any wasm queries fire.
    let err = execute(
        deps.as_mut(),
        mock_env(),
        message_info(&owner, &[]),
        ExecuteMsg::ZapBalance {
            input_denom: "inj".into(),
            max_spread: None,
            slippage_tolerance: None,
            min_lp_out: None,
            deadline: None,
        },
    )
    .unwrap_err();
    assert!(matches!(err, ContractError::BalanceBelowMin { .. }));
}

#[test]
fn add_remove_keeper_owner_only() {
    let mut deps = mock_deps_simple();
    do_instantiate(&mut deps);
    let stranger = deps.api.addr_make("stranger");
    let keeper = deps.api.addr_make("keeper");

    let err = execute(
        deps.as_mut(),
        mock_env(),
        message_info(&stranger, &[]),
        ExecuteMsg::AddKeeper {
            address: keeper.to_string(),
        },
    )
    .unwrap_err();
    assert!(matches!(err, ContractError::Unauthorized {}));
}

#[test]
fn allowlisted_keeper_passes_auth_check() {
    let mut deps = mock_deps_simple();
    let treasury = deps.api.addr_make("treasury");
    do_instantiate_with_defaults(
        &mut deps,
        Some(treasury.to_string()),
        Some(0),
        Some(Uint128::new(1_000_000)),
    );
    let keeper = deps.api.addr_make("keeper");
    let pair = deps.api.addr_make("pair").to_string();
    register_route(&mut deps, "inj", &pair);
    add_keeper(&mut deps, &keeper);

    // Below-min still fires (balance=0 from MockQuerier), but we expect the
    // BalanceBelowMin error — not NotKeeper — proving the keeper passed auth.
    let err = execute(
        deps.as_mut(),
        mock_env(),
        message_info(&keeper, &[]),
        ExecuteMsg::ZapBalance {
            input_denom: "inj".into(),
            max_spread: None,
            slippage_tolerance: None,
            min_lp_out: None,
            deadline: None,
        },
    )
    .unwrap_err();
    assert!(matches!(err, ContractError::BalanceBelowMin { .. }));
}

#[test]
fn revoked_keeper_loses_access() {
    let mut deps = mock_deps_simple();
    let treasury = deps.api.addr_make("treasury");
    do_instantiate_with_defaults(&mut deps, Some(treasury.to_string()), None, None);
    let keeper = deps.api.addr_make("keeper");
    add_keeper(&mut deps, &keeper);

    // Revoke.
    let owner = owner_addr(&deps.api);
    execute(
        deps.as_mut(),
        mock_env(),
        message_info(&owner, &[]),
        ExecuteMsg::RemoveKeeper {
            address: keeper.to_string(),
        },
    )
    .unwrap();

    let err = execute(
        deps.as_mut(),
        mock_env(),
        message_info(&keeper, &[]),
        ExecuteMsg::ZapBalance {
            input_denom: "inj".into(),
            max_spread: None,
            slippage_tolerance: None,
            min_lp_out: None,
            deadline: None,
        },
    )
    .unwrap_err();
    assert!(matches!(err, ContractError::NotKeeper {}));
}

#[test]
fn register_route_owner_only_and_overwritable() {
    let mut deps = mock_deps_simple();
    do_instantiate(&mut deps);
    let owner = owner_addr(&deps.api);
    let stranger = deps.api.addr_make("stranger");
    let pair_a = deps.api.addr_make("pair_a").to_string();
    let pair_b = deps.api.addr_make("pair_b").to_string();

    // Non-owner rejected.
    let err = execute(
        deps.as_mut(),
        mock_env(),
        message_info(&stranger, &[]),
        ExecuteMsg::RegisterRoute {
            input_denom: "inj".into(),
            pair: pair_a.clone(),
        },
    )
    .unwrap_err();
    assert!(matches!(err, ContractError::Unauthorized {}));

    // Owner registers, then overwrites — both succeed.
    execute(
        deps.as_mut(),
        mock_env(),
        message_info(&owner, &[]),
        ExecuteMsg::RegisterRoute {
            input_denom: "inj".into(),
            pair: pair_a.clone(),
        },
    )
    .unwrap();
    execute(
        deps.as_mut(),
        mock_env(),
        message_info(&owner, &[]),
        ExecuteMsg::RegisterRoute {
            input_denom: "inj".into(),
            pair: pair_b.clone(),
        },
    )
    .unwrap();
    let stored = crate::state::ROUTES.load(&deps.storage, "inj").unwrap();
    assert_eq!(stored.to_string(), pair_b);
}

// -------- tip cap --------

#[test]
fn instantiate_rejects_tip_over_cap() {
    let mut deps = mock_deps_simple();
    let owner = owner_addr(&deps.api);
    let err = instantiate(
        deps.as_mut(),
        mock_env(),
        message_info(&owner, &[]),
        InstantiateMsg {
            owner: None,
            default_recipient: None,
            tip_bps: Some(101),
            min_zap_amount: None,
        },
    )
    .unwrap_err();
    assert!(matches!(err, ContractError::TipTooHigh { value: 101, .. }));
}

#[test]
fn update_config_rejects_tip_over_cap() {
    let mut deps = mock_deps_simple();
    do_instantiate(&mut deps);
    let owner = owner_addr(&deps.api);
    let err = execute(
        deps.as_mut(),
        mock_env(),
        message_info(&owner, &[]),
        ExecuteMsg::UpdateConfig {
            owner: None,
            default_recipient: None,
            tip_bps: Some(500),
            min_zap_amount: None,
        },
    )
    .unwrap_err();
    assert!(matches!(err, ContractError::TipTooHigh { value: 500, .. }));
}

/// Drives `Callback::Sweep` directly with realistic balances and snapshots,
/// proving deltas leave the contract while pre-existing balances stay put.
/// MockQuerier's `update_balance` lets us pretend the bank module sees
/// whatever post-zap state we want.
#[test]
fn sweep_forwards_only_delta() {
    use cosmwasm_std::{BankMsg, CosmosMsg, SubMsg};

    let mut deps = mock_deps_simple();
    do_instantiate(&mut deps);
    let env = mock_env();

    let recipient = deps.api.addr_make("recipient");
    let denom_a = "inj".to_string();
    let denom_b = "usdt".to_string();
    let lp_denom = "factory/pair/lp".to_string();

    // Pre-existing baseline that must remain untouched.
    let pre_a = Uint128::new(900);
    let pre_b = Uint128::new(40_000);
    let pre_lp = Uint128::new(7);
    // Post-zap balance = baseline + delta the zap produced.
    let bal_a = pre_a + Uint128::new(3); // 3 dust A
    let bal_b = pre_b + Uint128::new(11); // 11 dust B
    let bal_lp = pre_lp + Uint128::new(123); // 123 freshly minted LP

    deps.querier.bank.update_balance(
        env.contract.address.clone(),
        vec![
            coin(bal_a.u128(), denom_a.clone()),
            coin(bal_b.u128(), denom_b.clone()),
            coin(bal_lp.u128(), lp_denom.clone()),
        ],
    );

    let res = execute(
        deps.as_mut(),
        env.clone(),
        message_info(&env.contract.address, &[]),
        ExecuteMsg::Callback(CallbackMsg::Sweep {
            recipient: recipient.to_string(),
            denom_a: denom_a.clone(),
            denom_b: denom_b.clone(),
            lp_denom: lp_denom.clone(),
            pre_a,
            pre_b,
            pre_lp,
            min_lp_out: None,
        }),
    )
    .unwrap();

    // One BankMsg::Send to recipient carrying exactly the three deltas.
    assert_eq!(res.messages.len(), 1);
    let SubMsg { msg, .. } = res.messages[0].clone();
    let bank_msg = match msg {
        CosmosMsg::Bank(b) => b,
        other => panic!("expected BankMsg, got {:?}", other),
    };
    let BankMsg::Send { to_address, amount } = bank_msg else {
        panic!("expected BankMsg::Send");
    };
    assert_eq!(to_address, recipient.to_string());
    // Sorted by denom; lookup by denom for clarity.
    let by_denom: std::collections::HashMap<_, _> =
        amount.iter().map(|c| (c.denom.as_str(), c.amount.u128())).collect();
    assert_eq!(by_denom.get(denom_a.as_str()), Some(&3));
    assert_eq!(by_denom.get(denom_b.as_str()), Some(&11));
    assert_eq!(by_denom.get(lp_denom.as_str()), Some(&123));

    // And we don't accidentally drain the baseline.
    let post_send_a = bal_a - Uint128::new(3);
    assert_eq!(post_send_a, pre_a);
}

#[test]
fn sweep_min_lp_out_uses_delta_not_total() {
    let mut deps = mock_deps_simple();
    do_instantiate(&mut deps);
    let env = mock_env();

    let lp_denom = "factory/pair/lp".to_string();
    let pre_lp = Uint128::new(1_000);
    let minted = Uint128::new(10);
    let bal_lp = pre_lp + minted;

    deps.querier.bank.update_balance(
        env.contract.address.clone(),
        vec![coin(bal_lp.u128(), lp_denom.clone())],
    );

    // Caller asks for min_lp_out=50; the delta is only 10, so this must reject
    // even though `bal_lp` itself is 1010.
    let recipient = deps.api.addr_make("r").to_string();
    let err = execute(
        deps.as_mut(),
        env.clone(),
        message_info(&env.contract.address, &[]),
        ExecuteMsg::Callback(CallbackMsg::Sweep {
            recipient,
            denom_a: "inj".into(),
            denom_b: "usdt".into(),
            lp_denom,
            pre_a: Uint128::zero(),
            pre_b: Uint128::zero(),
            pre_lp,
            min_lp_out: Some(Uint128::new(50)),
        }),
    )
    .unwrap_err();
    assert!(matches!(err, ContractError::MinLpAssertion { .. }));
}

// -------- execute: auth --------

#[test]
fn callback_rejects_external_sender() {
    let mut deps = mock_deps_simple();
    do_instantiate(&mut deps);
    let attacker = deps.api.addr_make("attacker");
    let treasury = deps.api.addr_make("treasury").to_string();

    let err = execute(
        deps.as_mut(),
        mock_env(),
        message_info(&attacker, &[]),
        ExecuteMsg::Callback(CallbackMsg::Sweep {
            recipient: treasury,
            denom_a: "inj".into(),
            denom_b: "usdt".into(),
            lp_denom: "factory/pair/lp".into(),
            pre_a: Uint128::zero(),
            pre_b: Uint128::zero(),
            pre_lp: Uint128::zero(),
            min_lp_out: None,
        }),
    )
    .unwrap_err();
    assert!(matches!(err, ContractError::Unauthorized {}));
}

#[test]
fn update_config_owner_only() {
    let mut deps = mock_deps_simple();
    do_instantiate(&mut deps);
    let stranger = deps.api.addr_make("stranger");
    let new_treasury = deps.api.addr_make("new_treasury").to_string();

    let err = execute(
        deps.as_mut(),
        mock_env(),
        message_info(&stranger, &[]),
        ExecuteMsg::UpdateConfig {
            owner: None,
            default_recipient: Some(new_treasury),
            tip_bps: None,
            min_zap_amount: None,
        },
    )
    .unwrap_err();
    assert!(matches!(err, ContractError::Unauthorized {}));
}

#[test]
fn update_config_owner_can_update() {
    let mut deps = mock_deps_simple();
    do_instantiate(&mut deps);
    let owner = owner_addr(&deps.api);
    let new_treasury = deps.api.addr_make("new_treasury");
    let new_treasury_str = new_treasury.to_string();

    execute(
        deps.as_mut(),
        mock_env(),
        message_info(&owner, &[]),
        ExecuteMsg::UpdateConfig {
            owner: None,
            default_recipient: Some(new_treasury_str),
            tip_bps: Some(50),
            min_zap_amount: Some(Uint128::new(123_456)),
        },
    )
    .unwrap();

    let config = crate::state::CONFIG.load(&deps.storage).unwrap();
    assert_eq!(config.default_recipient, Some(new_treasury));
    assert_eq!(config.tip_bps, 50);
    assert_eq!(config.min_zap_amount, Uint128::new(123_456));
}

#[test]
fn update_config_empty_default_clears() {
    let mut deps = mock_deps_simple();
    do_instantiate(&mut deps);
    let owner = owner_addr(&deps.api);
    let treasury = deps.api.addr_make("t").to_string();

    execute(
        deps.as_mut(),
        mock_env(),
        message_info(&owner, &[]),
        ExecuteMsg::UpdateConfig {
            owner: None,
            default_recipient: Some(treasury),
            tip_bps: None,
            min_zap_amount: None,
        },
    )
    .unwrap();
    execute(
        deps.as_mut(),
        mock_env(),
        message_info(&owner, &[]),
        ExecuteMsg::UpdateConfig {
            owner: None,
            default_recipient: Some(String::new()),
            tip_bps: None,
            min_zap_amount: None,
        },
    )
    .unwrap();
    let config = crate::state::CONFIG.load(&deps.storage).unwrap();
    assert_eq!(config.default_recipient, None);
}
