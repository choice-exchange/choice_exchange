//! Standalone harness proving `injective-test-tube 1.19.0`'s bundled chain
//! (injective-core v1.19.0) can execute `injective.erc20.v1beta1.MsgCreateTokenPair`
//! and auto-deploy a `MintBurnBankERC20` for an empty `erc20_address` — the
//! exact submsg `choice_mts_issuer::RegisterLaunch` emits.
//!
//! Standalone (outside the choice_exchange workspace) on purpose: test-tube
//! 1.19 pulls cosmwasm-std 3 + injective-cosmwasm 0.3.6, which cargo would
//! unify onto the contract's 0.3.4-1 / cw-std 2.2.2 and break its compile.
//! Here we only need the chain, so there's no contract dependency.

use cosmwasm_std::Coin;
use injective_std::types::injective::erc20::v1beta1::{
    MsgCreateTokenPair, MsgCreateTokenPairResponse, QueryTokenPairByDenomRequest, TokenPair,
};
use injective_std::types::cosmos::base::v1beta1::Coin as ProtoCoin;
use injective_std::types::injective::tokenfactory::v1beta1::{MsgCreateDenom, MsgMint};
use injective_test_tube::{
    Account, Erc20, FeeSetting, InjectiveTestApp, Module, Runner, TokenFactory,
};

const CREATE_TOKEN_PAIR_URL: &str = "/injective.erc20.v1beta1.MsgCreateTokenPair";

#[test]
fn create_token_pair_autodeploys_erc20() {
    let app = InjectiveTestApp::new();
    // 1_000_000 INJ — well above any tokenfactory denom-creation fee.
    let signer = app
        .init_account(&[Coin::new(1_000_000_000_000_000_000_000_000u128, "inj")])
        .unwrap();

    // 1. Create a tokenfactory denom owned by `signer` (mirrors the issuer's
    //    MsgCreateDenom submsg, allow_admin_burn included).
    let tf = TokenFactory::new(&app);
    tf.create_denom(
        MsgCreateDenom {
            sender: signer.address(),
            subdenom: "shroomtest".to_string(),
            name: "Shroom Test".to_string(),
            symbol: "SHROOMT".to_string(),
            decimals: 18,
            allow_admin_burn: true,
        },
        &signer,
    )
    .unwrap();
    let denom = format!("factory/{}/shroomtest", signer.address());

    // 1b. Mint non-zero supply (the issuer mints `total_supply` before pairing;
    //     the erc20 module rejects pairing a zero-supply denom).
    tf.mint(
        MsgMint {
            sender: signer.address(),
            amount: Some(ProtoCoin {
                denom: denom.clone(),
                amount: "1000000000000000000000000000".to_string(), // 1e27
            }),
            receiver: signer.address(),
        },
        &signer,
    )
    .unwrap();

    // 2. The message under test: pair the denom with an auto-deployed ERC20
    //    (erc20_address = "" → chain deploys MintBurnBankERC20 owned by the
    //    lower-20-byte form of `sender`). The auto-deploy is an internal
    //    MsgEthereumTx, so the cosmos tx needs a gas limit big enough to cover
    //    EVM contract creation — Auto simulation under-provisions it.
    let signer = signer.with_fee_setting(FeeSetting::Custom {
        amount: Coin::new(5_000_000_000_000_000u128, "inj"), // 0.005 INJ fee
        gas_limit: 60_000_000,
    });
    // The EVM module emits event attributes with binary (non-UTF-8) values on
    // the auto-deploy, which test-tube-inj 2.0.10's response decoder panics on
    // while parsing the *already-committed* FinalizeBlock events. The state
    // change is durable regardless, so we swallow that decode panic and treat
    // the query below as the source of truth.
    let prev_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let exec = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        app.execute::<_, MsgCreateTokenPairResponse>(
            MsgCreateTokenPair {
                sender: signer.address(),
                token_pair: Some(TokenPair {
                    bank_denom: denom.clone(),
                    erc20_address: String::new(),
                }),
            },
            CREATE_TOKEN_PAIR_URL,
            &signer,
        )
    }));
    std::panic::set_hook(prev_hook);
    match &exec {
        Ok(Ok(r)) => println!(
            "execute returned cleanly; erc20 = {:?}",
            r.data.token_pair.as_ref().map(|p| &p.erc20_address)
        ),
        Ok(Err(e)) => panic!("MsgCreateTokenPair rejected by chain: {e:?}"),
        Err(_) => println!(
            "execute committed but response-event decode panicked (known \
             test-tube EVM-event UTF-8 limitation) — verifying via query"
        ),
    }

    // 3. Source of truth: query the erc20 module. A non-empty erc20_address
    //    proves MsgCreateTokenPair executed AND the chain auto-deployed the
    //    MintBurnBankERC20. (Query responses are protobuf, not raw block
    //    events, so they avoid the non-UTF-8 decode bug above.)
    let q = Erc20::new(&app);
    let by_denom = q
        .query_token_pair_by_denom(&QueryTokenPairByDenomRequest {
            bank_denom: denom.clone(),
        })
        .unwrap();
    let erc20 = by_denom
        .token_pair
        .expect("queried pair missing — MsgCreateTokenPair did not persist")
        .erc20_address;
    assert!(
        erc20.starts_with("0x") && erc20.len() == 42,
        "expected a 20-byte 0x ERC20 address, got {erc20:?}"
    );
    println!("queried pair for {denom}\n  -> auto-deployed erc20 = {erc20}  OK");
}
