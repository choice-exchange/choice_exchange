//! Shared helpers for the chain-capability integration tests. See README.md.

use bech32::{FromBase32, ToBase32, Variant};
use cosmwasm_std::Coin;
use injective_std::types::cosmos::bank::v1beta1::{MsgSend, QueryBalanceRequest};
use injective_std::types::cosmos::base::v1beta1::Coin as ProtoCoin;
use injective_std::types::cosmwasm::wasm::v1::MsgExecuteContractResponse;
use injective_test_tube::{
    Account, Bank, ExecuteResponse, FeeSetting, InjectiveTestApp, Module, SigningAccount,
};
use sha2::{Digest, Sha256};

/// 1e24 inj — generous per-account funding.
pub const FUND: u128 = 1_000_000_000_000_000_000_000_000;

/// Read a compiled contract artifact from the workspace `artifacts/` dir
/// (relative to the harness crate root, which is the cwd during `cargo test`).
pub fn artifact(name: &str) -> Vec<u8> {
    let path = format!("../artifacts/{name}.wasm");
    std::fs::read(&path)
        .unwrap_or_else(|_| panic!("missing artifact {path} — run build_release.sh first"))
}

pub fn sha256(data: &[u8]) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(data);
    h.finalize().into()
}

/// bech32-decode an Injective address to its 20-byte canonical form.
pub fn canon20(addr: &str) -> Vec<u8> {
    let (_hrp, data, _v) = bech32::decode(addr).expect("bech32 decode");
    Vec::<u8>::from_base32(&data).expect("from_base32")
}

/// bech32-encode 20 canonical bytes back to an `inj1…` address.
pub fn humanize20(bytes: &[u8]) -> String {
    bech32::encode("inj", bytes.to_base32(), Variant::Bech32).expect("bech32 encode")
}

/// wasmd `MsgInstantiateContract2` address, Injective fork (sha256 hash
/// truncated to the first 20 bytes). Mirrors
/// `keeper/src/addressing/instantiate2.ts`. Empty instantiate msg
/// (contract-emitted Instantiate2 → FixMsg=false).
pub fn instantiate2_addr(checksum: &[u8], creator_canon: &[u8], salt: &[u8]) -> String {
    let inner = sha256(b"module");
    let mut key: Vec<u8> = Vec::new();
    key.extend_from_slice(b"wasm");
    key.push(0u8);
    key.extend_from_slice(&(checksum.len() as u64).to_be_bytes());
    key.extend_from_slice(checksum);
    key.extend_from_slice(&(creator_canon.len() as u64).to_be_bytes());
    key.extend_from_slice(creator_canon);
    key.extend_from_slice(&(salt.len() as u64).to_be_bytes());
    key.extend_from_slice(salt);
    key.extend_from_slice(&(0u64).to_be_bytes()); // empty msg
    let mut h = Sha256::new();
    h.update(inner);
    h.update(&key);
    let full: [u8; 32] = h.finalize().into();
    humanize20(&full[..20])
}

/// Issuer salt convention: `issuer_canonical(20) || be_u64(internal_id)`.
pub fn issuer_salt(issuer: &str, internal_id: u64) -> Vec<u8> {
    let mut s = canon20(issuer);
    s.extend_from_slice(&internal_id.to_be_bytes());
    s
}

/// Run `f` (a chain-mutating execute), swallowing the known test-tube-inj
/// 2.0.10 panic on decoding non-UTF-8 EVM FinalizeBlock event attributes. The
/// block is already committed when that fires, so callers verify via queries.
pub fn exec_tolerant<F: FnOnce()>(f: F) {
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
    std::panic::set_hook(prev);
}

/// Re-sign an account with a high custom gas limit — required for any tx that
/// triggers the inner `MsgEthereumTx` ERC20 auto-deploy (Auto fee-simulation
/// under-provisions it). 120M stays under the 150M block cap.
pub fn custom_gas(acct: SigningAccount) -> SigningAccount {
    acct.with_fee_setting(FeeSetting::Custom {
        amount: Coin::new(500_000_000_000_000_000u128, "inj"), // 0.5 INJ
        gas_limit: 120_000_000,
    })
}

/// Send native coins between accounts.
pub fn bank_send(app: &InjectiveTestApp, from: &SigningAccount, to: &str, denom: &str, amount: u128) {
    Bank::new(app)
        .send(
            MsgSend {
                from_address: from.address(),
                to_address: to.to_string(),
                amount: vec![ProtoCoin {
                    denom: denom.to_string(),
                    amount: amount.to_string(),
                }],
            },
            from,
        )
        .unwrap();
}

/// Pull the `_contract_address` emitted by an `instantiate` event — recovers the
/// Instantiate2 address of a sink/locker a factory just spawned.
pub fn instantiated_addr(res: &ExecuteResponse<MsgExecuteContractResponse>) -> String {
    res.events
        .iter()
        .find(|e| e.ty == "instantiate")
        .and_then(|e| e.attributes.iter().find(|a| a.key == "_contract_address"))
        .map(|a| a.value.clone())
        .expect("instantiate event with _contract_address attribute")
}

pub fn bank_balance(bank: &Bank<InjectiveTestApp>, addr: &str, denom: &str) -> String {
    bank.query_balance(&QueryBalanceRequest {
        address: addr.to_string(),
        denom: denom.to_string(),
    })
    .unwrap()
    .balance
    .map(|c| c.amount)
    .unwrap_or_else(|| "0".to_string())
}
