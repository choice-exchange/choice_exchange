use cosmwasm_std::testing::{message_info, mock_env, MockApi, MockQuerier, MockStorage};
use cosmwasm_std::{coin, coins, to_json_binary, Addr, Empty, OwnedDeps, Uint128};
use std::marker::PhantomData;

use cw20::Cw20ReceiveMsg;
use injective_cosmwasm::query::InjectiveQueryWrapper;

use choice::asset::AssetInfo;

use crate::contract::{execute, instantiate, migrate};
use crate::error::ContractError;
use crate::math::{isqrt, optimal_swap_in, simulate_swap_return};
use crate::msg::{CallbackMsg, ExecuteMsg, InstantiateMsg, MigrateMsg, ZapHookMsg};

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

fn native(denom: &str) -> AssetInfo {
    AssetInfo::NativeToken {
        denom: denom.to_string(),
    }
}

fn cw20(addr: &Addr) -> AssetInfo {
    AssetInfo::Token {
        contract_addr: addr.to_string(),
    }
}

/// Default route used by tests that don't otherwise care: native INJ routed
/// into a deterministic mock pair address. The royalty (input, pair) pair is
/// required at instantiate time as of v2, so every test threads it through.
fn default_route(api: &MockApi) -> (AssetInfo, String) {
    (native("inj"), api.addr_make("pair").to_string())
}

fn do_instantiate(
    deps: &mut OwnedDeps<MockStorage, MockApi, MockQuerier<Empty>, InjectiveQueryWrapper>,
) {
    let (input, pair) = default_route(&deps.api);
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
            input,
            pair,
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
    let (input, pair) = default_route(&deps.api);
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
            input,
            pair,
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

    // Regression: pre-fix isqrt wrapped `Decimal256::sqrt`, which scales the
    // input by 10^18 and panicked on radicands above ~1.16e59. Exercise a
    // value well past that threshold (10^70) — the Decimal256-based code path
    // would have hit `panic!("Multiplication overflow")` in `from_ratio`.
    let huge = Uint256::from(10u128).pow(70);
    let huge_root = Uint256::from(10u128).pow(35);
    assert_eq!(isqrt(huge).unwrap(), huge_root);
}

/// Regression: zapping into a pair whose `r_a^2 * 4e6` exceeds ~1.16e59 used
/// to panic inside the pre-fix isqrt. A pair holding 10^28 wei (10B units of
/// an 18-decimal token) hits that ceiling; this case should now return cleanly.
#[test]
fn optimal_split_does_not_overflow_on_large_pair() {
    let r_a = Uint128::new(10_000_000_000_000_000_000_000_000_000u128); // 1e28
    let x_in = Uint128::new(1_000_000_000_000_000_000_000u128); // 1e21
    let s = optimal_swap_in(r_a, x_in).expect("should not panic on large radicand");
    assert!(!s.is_zero());
    assert!(s <= x_in);
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

#[test]
fn optimal_split_deep_pool_is_just_over_half() {
    let r_a = Uint128::new(1_000_000_000_000_000_000);
    let x = Uint128::new(1_000_000_000_000);
    let s = optimal_swap_in(r_a, x).unwrap();
    let half = x.u128() / 2;
    assert!(s.u128() >= half, "s {} should be >= half {}", s, half);
    let upper = half + half / 200;
    assert!(s.u128() <= upper, "s {} should be <= {} (half+0.5%)", s, upper);
}

#[test]
fn optimal_split_balances_post_swap_ratio() {
    let r_a = Uint128::new(5_000_000_000_000);
    let r_b = Uint128::new(2_000_000_000_000);
    let x = Uint128::new(100_000_000);

    let s = optimal_swap_in(r_a, x).unwrap();
    let b = simulate_swap_return(r_a, r_b, s).unwrap();
    let remaining = x - s;

    let r_a_post = r_a + s;
    let r_b_post = r_b - b;

    let lhs = remaining.u128() * r_b_post.u128();
    let rhs = b.u128() * r_a_post.u128();
    let diff = lhs.abs_diff(rhs);
    let tolerance = lhs / 100;
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

// -------- Receive (CW20 entry point) --------

#[test]
fn receive_rejects_funds_attached() {
    let mut deps = mock_deps_simple();
    do_instantiate(&mut deps);
    let cw20_contract = deps.api.addr_make("cw20");
    let user = deps.api.addr_make("user");
    let pair = deps.api.addr_make("pair").to_string();

    let hook = to_json_binary(&ZapHookMsg::Zap {
        pair,
        recipient: None,
        max_spread: None,
        slippage_tolerance: None,
        min_lp_out: None,
        deadline: None,
    })
    .unwrap();

    let err = execute(
        deps.as_mut(),
        mock_env(),
        message_info(&cw20_contract, &coins(1, "inj")),
        ExecuteMsg::Receive(Cw20ReceiveMsg {
            sender: user.to_string(),
            amount: Uint128::new(1_000),
            msg: hook,
        }),
    )
    .unwrap_err();
    assert!(matches!(err, ContractError::ReceiveWithFunds { count: 1 }));
}

#[test]
fn receive_rejects_zero_amount() {
    let mut deps = mock_deps_simple();
    do_instantiate(&mut deps);
    let cw20_contract = deps.api.addr_make("cw20");
    let user = deps.api.addr_make("user");
    let pair = deps.api.addr_make("pair").to_string();

    let hook = to_json_binary(&ZapHookMsg::Zap {
        pair,
        recipient: None,
        max_spread: None,
        slippage_tolerance: None,
        min_lp_out: None,
        deadline: None,
    })
    .unwrap();

    let err = execute(
        deps.as_mut(),
        mock_env(),
        message_info(&cw20_contract, &[]),
        ExecuteMsg::Receive(Cw20ReceiveMsg {
            sender: user.to_string(),
            amount: Uint128::zero(),
            msg: hook,
        }),
    )
    .unwrap_err();
    assert!(matches!(err, ContractError::ZeroInputAmount {}));
}

#[test]
fn receive_rejects_expired_deadline() {
    let mut deps = mock_deps_simple();
    do_instantiate(&mut deps);
    let cw20_contract = deps.api.addr_make("cw20");
    let user = deps.api.addr_make("user");
    let pair = deps.api.addr_make("pair").to_string();
    let env = mock_env();

    let hook = to_json_binary(&ZapHookMsg::Zap {
        pair,
        recipient: None,
        max_spread: None,
        slippage_tolerance: None,
        min_lp_out: None,
        deadline: Some(env.block.time.seconds()),
    })
    .unwrap();

    let err = execute(
        deps.as_mut(),
        env,
        message_info(&cw20_contract, &[]),
        ExecuteMsg::Receive(Cw20ReceiveMsg {
            sender: user.to_string(),
            amount: Uint128::new(1_000),
            msg: hook,
        }),
    )
    .unwrap_err();
    assert!(matches!(err, ContractError::ExpiredDeadline {}));
}

// -------- ZapBalance --------

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
    do_instantiate(&mut deps);
    let owner = owner_addr(&deps.api);

    let err = execute(
        deps.as_mut(),
        mock_env(),
        message_info(&owner, &[]),
        ExecuteMsg::ZapBalance {
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
fn zap_balance_below_min_aborts() {
    let mut deps = mock_deps_simple();
    let treasury = deps.api.addr_make("treasury");
    do_instantiate_with_defaults(
        &mut deps,
        Some(treasury.to_string()),
        Some(0),
        Some(Uint128::new(1_000_000)),
    );
    let owner = owner_addr(&deps.api);

    // MockQuerier returns 0 for unset balances — strictly below the 1_000_000
    // min, so we expect BalanceBelowMin before any wasm queries fire.
    let err = execute(
        deps.as_mut(),
        mock_env(),
        message_info(&owner, &[]),
        ExecuteMsg::ZapBalance {
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
    add_keeper(&mut deps, &keeper);

    // Keeper auth passes; we then short-circuit on BalanceBelowMin (no funds
    // staged in the mock).
    let err = execute(
        deps.as_mut(),
        mock_env(),
        message_info(&keeper, &[]),
        ExecuteMsg::ZapBalance {
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
            max_spread: None,
            slippage_tolerance: None,
            min_lp_out: None,
            deadline: None,
        },
    )
    .unwrap_err();
    assert!(matches!(err, ContractError::NotKeeper {}));
}

// -------- tip cap --------

#[test]
fn instantiate_rejects_tip_over_cap() {
    let mut deps = mock_deps_simple();
    let (input, pair) = default_route(&deps.api);
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
            input,
            pair,
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

// -------- callback Sweep with mocked balances --------

#[test]
fn sweep_forwards_only_delta_native_native() {
    use cosmwasm_std::{BankMsg, CosmosMsg, SubMsg};

    let mut deps = mock_deps_simple();
    do_instantiate(&mut deps);
    let env = mock_env();

    let recipient = deps.api.addr_make("recipient");
    let lp_denom = "factory/pair/lp".to_string();

    let pre_a = Uint128::new(900);
    let pre_b = Uint128::new(40_000);
    let pre_lp = Uint128::new(7);
    let bal_a = pre_a + Uint128::new(3);
    let bal_b = pre_b + Uint128::new(11);
    let bal_lp = pre_lp + Uint128::new(123);

    deps.querier.bank.update_balance(
        env.contract.address.clone(),
        vec![
            coin(bal_a.u128(), "inj"),
            coin(bal_b.u128(), "usdt"),
            coin(bal_lp.u128(), lp_denom.clone()),
        ],
    );

    let res = execute(
        deps.as_mut(),
        env.clone(),
        message_info(&env.contract.address, &[]),
        ExecuteMsg::Callback(CallbackMsg::Sweep {
            recipient: recipient.to_string(),
            asset_a: native("inj"),
            asset_b: native("usdt"),
            lp_denom: lp_denom.clone(),
            pre_a,
            pre_b,
            pre_lp,
            min_lp_out: None,
        }),
    )
    .unwrap();

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
    let by_denom: std::collections::HashMap<_, _> =
        amount.iter().map(|c| (c.denom.as_str(), c.amount.u128())).collect();
    assert_eq!(by_denom.get("inj"), Some(&3));
    assert_eq!(by_denom.get("usdt"), Some(&11));
    assert_eq!(by_denom.get(lp_denom.as_str()), Some(&123));
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

    let recipient = deps.api.addr_make("r").to_string();
    let err = execute(
        deps.as_mut(),
        env.clone(),
        message_info(&env.contract.address, &[]),
        ExecuteMsg::Callback(CallbackMsg::Sweep {
            recipient,
            asset_a: native("inj"),
            asset_b: native("usdt"),
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
            asset_a: native("inj"),
            asset_b: native("usdt"),
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

// -------- migrate v1 → v2 --------

#[test]
fn migrate_v1_to_v2_preserves_config_and_pins_route() {
    use cosmwasm_std::Storage;

    let mut deps = mock_deps_simple();
    let owner = owner_addr(&deps.api);
    let treasury = deps.api.addr_make("treasury");

    // Stage a v1-shaped Config directly under the `b"config"` key (the v2
    // Config struct adds `input`/`pair`, which weren't present in v1).
    let v1_json = serde_json::json!({
        "owner": owner.to_string(),
        "default_recipient": treasury.to_string(),
        "tip_bps": 25u16,
        "min_zap_amount": "1000",
    });
    deps.storage
        .set(b"config", v1_json.to_string().as_bytes());

    let pair = deps.api.addr_make("royalty_pair");
    let token = deps.api.addr_make("royalty_cw20");

    migrate(
        deps.as_mut(),
        mock_env(),
        MigrateMsg {
            input: cw20(&token),
            pair: pair.to_string(),
        },
    )
    .unwrap();

    let config = crate::state::CONFIG.load(&deps.storage).unwrap();
    // v1 fields survive.
    assert_eq!(config.owner, owner);
    assert_eq!(config.default_recipient, Some(treasury));
    assert_eq!(config.tip_bps, 25);
    assert_eq!(config.min_zap_amount, Uint128::new(1_000));
    // v2 fields come from MigrateMsg.
    assert_eq!(config.input, cw20(&token));
    assert_eq!(config.pair, pair);
}

// -------- admin Sweep with CW20 --------

#[test]
fn admin_sweep_emits_cw20_transfer_for_cw20_assets() {
    use cosmwasm_std::{from_json as cw_from_json, CosmosMsg, WasmMsg};

    let mut deps = mock_deps_simple();
    do_instantiate(&mut deps);
    let owner = owner_addr(&deps.api);
    let token = deps.api.addr_make("token");
    let recipient = deps.api.addr_make("recipient");

    let token_str = token.to_string();
    deps.querier
        .update_wasm(move |q| match q {
            cosmwasm_std::WasmQuery::Smart { contract_addr, .. }
                if contract_addr == &token_str =>
            {
                cosmwasm_std::SystemResult::Ok(cosmwasm_std::ContractResult::Ok(
                    to_json_binary(&cw20::BalanceResponse {
                        balance: Uint128::new(500),
                    })
                    .unwrap(),
                ))
            }
            _ => cosmwasm_std::SystemResult::Err(cosmwasm_std::SystemError::NoSuchContract {
                addr: "unknown".to_string(),
            }),
        });

    let resp = execute(
        deps.as_mut(),
        mock_env(),
        message_info(&owner, &[]),
        ExecuteMsg::Sweep {
            recipient: recipient.to_string(),
            assets: vec![cw20(&token)],
        },
    )
    .unwrap();

    // One Wasm Cw20::Transfer to recipient for 500.
    assert_eq!(resp.messages.len(), 1);
    let CosmosMsg::Wasm(WasmMsg::Execute {
        contract_addr,
        msg,
        funds,
    }) = resp.messages[0].msg.clone()
    else {
        panic!("expected Wasm::Execute");
    };
    assert_eq!(contract_addr, token.to_string());
    assert!(funds.is_empty());
    let transfer: cw20::Cw20ExecuteMsg = cw_from_json(&msg).unwrap();
    match transfer {
        cw20::Cw20ExecuteMsg::Transfer { recipient: r, amount } => {
            assert_eq!(r, recipient.to_string());
            assert_eq!(amount, Uint128::new(500));
        }
        other => panic!("expected Transfer, got {:?}", other),
    }
}
