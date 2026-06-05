# Handoff: zap dust-sweep fix for contract recipients (erc20 EVM-hook panic)

Date: 2026-06-03. Pick-up doc for a fresh session. Cross-session memory:
`project_choice_zap_inj_usdc_royalty`.

---
## ⛔ RESOLVED 2026-06-04 — and the premise of THIS doc was WRONG

The actual fix was **NOT** a contract change. The `transfer is restricted by EVM
hook ... restricted action` (code 9) was an **out-of-gas** in disguise: the keeper
broadcast with a ~1.2M gas limit (`MsgBroadcasterWithPk` doesn't simulate by
default), but the zap needs ~1.9M. When it OOG'd, the permissions module's EVM
hook (`isTransferRestricted` on the USDC erc20) couldn't get gas and **failed
closed**, emitting the misleading "restricted" string. See chain source
`tools/register-mts/injective-core/.../permissions/keeper/evm_hook.go:178`.

PROOF erc20→contract was never the problem: successful zap `8DCBE0…` bank-sends
erc20 straight to the Treasury **contract** `inj1c2yle…` and passes the hook. The
only difference in the failing keeper tx was gas (1.2M wanted/used = OOG) vs the
successful 3.5M.

**FIX (live + validated):** keeper `ZAP_GAS_LIMIT` (default 3.5M) passed via
`broadcast({msgs, gas:{gas}})` — `choice-zap-keeper` src/{config,index}.ts,
rsync-deployed. Zap `71B11973…` code 0: 2 INJ → LP to Treasury, keeper tipped,
contract drained, idle.

The contract change + migrate to **code 2030** below DID happen but was
unnecessary; it's harmless (retains 1 erc20 base-unit dust per zap in `inj1srnm…`).
Everything from here down is the original (mistaken) erc20-recipient theory, kept
for history.
---

## Problem (root cause, proven on mainnet)

The mainnet royalty zap instance `inj1srnm0dzys640j8t75tc9l37aen6m6lagqfzzct`
(code id 2002, v2.0.0) auto-zaps INJ royalties into the INJ/USDC pair
`inj1583lyh82kgflpwe25lsvsmw5t343hawyef5ppw`. Its `default_recipient` is the
**Choice Treasury Multisig** — which is a **cw3 CONTRACT**
(`inj1c2yleauy9say73tsx3dk5tvlgwwzdh96r76zv4`).

`callback_sweep` (`src/contract.rs` ~L727) bundles the minted LP **plus the
freshly produced dust** into one `BankMsg::Send` to the recipient (L778). Choice
"USDC" is the EVM-native token `erc20:0xa00C59fF5a080D2b954d0c75e46E22a0c371235a`.
Bank-sending an `erc20:` denom **to a contract** from inside the zap's nested
execution trips the bank→EVM hook and panics:

```
transfer is restricted by EVM hook: panic during EVM hook:
{EVM hook call failed}: contract hook query error: restricted action   (code 9)
```

So `ZapBalance` reverts every time (burned ~1.2M gas/attempt). Evidence:
- Frontend `Zap` → **EOA** recipient succeeded on this exact pair (tx `0B52DA…`).
- Permissionless `Zap` on the **new** instance `inj1srnm…` → **EOA** recipient
  succeeded (tx `8DCBE0…`, code 0); it swept `dust_b=1` erc20 USDC to the EOA fine.
- Failed keeper `ZapBalance` → Treasury **contract** recipient (tx `127C8DF…`).
- The Treasury contract *does* hold 21 USDC (received via non-nested paths), so it
  is NOT "can't hold erc20" — it's specifically the nested bank-send-to-contract.

**Two use cases must both keep working:**
1. Royalty `ZapBalance` — recipient is the Treasury *contract*.
2. Frontend user `Zap`/`Receive` — recipient is the user's *EOA*; **users must
   still get their dust back.**

## The fix (contract change in `callback_sweep`)

Decide dust disposition by recipient type:
- **Recipient is a wasm contract** → send LP + native/INJ dust (hook-free), but
  **retain `erc20:`-prefixed dust in the zap contract** (do not bank-send it).
- **Recipient is an EOA** → unchanged: send LP + all dust (frontend users keep
  full dust return).

Detection: `deps.querier.query_wasm_contract_info(&recipient_addr)` → `Ok` ⇒
contract, `Err` ⇒ EOA. Apply in `callback_sweep` / `push_dust` when building the
native `bank_coins`: skip a coin when `recipient_is_contract && denom.starts_with("erc20:")`.
Only `erc20:`-class dust to contract recipients changes; peggy/factory/CW20/native
dust and all EOA-recipient behavior are untouched. CW20 dust (Cw20::Transfer) and
the LP (factory denom) are not EVM-hooked — leave them as-is.

Retained erc20 dust (~1 base unit ≈ $0.000001 per zap) accumulates harmlessly in
the zap contract; owner can `Sweep` it **to an EOA** later (Sweep to the treasury
contract would hit the same hook).

### Implementation notes
- File: `contracts/choice_zap_lp/src/contract.rs`, fn `callback_sweep` (~L727) and
  helper `push_dust` (used at L775-776). `asset_a`/`asset_b` are `AssetInfo`; the
  `erc20:` denom is a `NativeToken{denom}` whose `denom` starts with `erc20:`.
- Querier type is `QuerierWrapper<InjectiveQueryWrapper>` — it still exposes
  `query_wasm_contract_info`.
- Keep the change minimal; don't alter the LP / native-dust path.

### Tests (`tests/zap_lp_integration.rs`)
- New: `ZapBalance`-style zap with a **contract** recipient on an erc20-side pair →
  succeeds, LP delivered, erc20 dust stays in the zap contract.
- Regression: EOA recipient still receives erc20 dust.
- Run: `make build-all` first, then `cargo test --test zap_lp_integration`.
  (Integration tests use `injective_test_tube`; confirm it models the erc20 EVM
  hook — if it does not, the contract-recipient revert can't be reproduced in-tube,
  so rely on a mainnet re-test of the 2 INJ after migrate.)

## Rollout

1. ✅ Implement + test (above) — done 2026-06-04. `callback_sweep` retains `erc20:`
   dust for contract recipients; `push_dust` gained `recipient_is_contract` + returns
   retained amount; 2 new unit tests (`sweep_retains_erc20_dust_for_contract_recipient`,
   `sweep_forwards_erc20_dust_for_eoa_recipient`). 38/38 lib tests green.
2. ✅ Build: `./build_release.sh` → `artifacts/choice_zap_lp.wasm`
   sha256 `26b49bac3cf9f9768eeb0b689e0b2c397ac0bf1dabcaac09c5343e51709a8bf9`.
3. ✅ **Stored on mainnet** 2026-06-04 → **code id 2030** (tx
   `CD65F8F773E876910C657EA9135F2AADA5B455C99DC5F57010105721A2CC590C`, signer
   `choicedev` = `inj1yrg4pg8hcu0sw5rjlrcqfmw2ewf2uztlmdysak`). On-chain `data_hash`
   verified == local artifact hash above.
4. ⏳ **Migrate ONLY the royalty instance** (NOT the UI instance `inj17tvqalm…`).
   Wasm-admin = Dev Multisig, so this is a multisig `MsgMigrateContract`:
   contract `inj1srnm0dzys640j8t75tc9l37aen6m6lagqfzzct`, new code id from step 3,
   msg `{"patch":{}}` (`MigrateMsg::Patch` = v2→v2, bumps cw2 version, leaves Config
   untouched — `default_recipient` stays the Treasury contract).
5. **No `UpdateConfig`** — `default_recipient` is already the Treasury contract.
6. Restart the keeper and confirm the stuck **2 INJ** zaps to the treasury:
   ```
   ssh choice 'bash -lc "source /root/.nvm/nvm.sh && nvm use 24 && \
     pm2 start /root/bots/choice-zap-keeper/deploy/choice.config.cjs --update-env && pm2 save"'
   pm2 logs choice-zap-keeper   # expect firing ZapBalance → broadcast ok
   ```
   Verify: Treasury `inj1c2yle…` receives `factory/inj1583…/lp`; contract INJ → ~0;
   keeper got the 25bps tip; a little erc20 dust retained in `inj1srnm…`.

## Current state at handoff
- Keeper: pm2 process **stopped + DELETED** on `choice` (was revert-looping and
  burning gas). `.env` intact (`/root/bots/choice-zap-keeper/.env`); recipient lives
  in contract Config, not the keeper env, so no keeper config change is needed.
  Keeper hot key `inj1wt8yrdajp6h65qedunmurqv9nypzlm839eck66` ≈ 0.097 INJ gas.
- 2 INJ sits in `inj1srnm…` (safe; zaps after migrate).
- Diagnostic left an LP position + erc20 dust in EOA `inj1q2m26a…` (deploy key).
- Uncommitted: `deploy/instantiate_zap_lp.sh` (v2 route-pin patch) +
  `deployed_contracts.md` (royalty-stream entry) on `choice_exchange`
  @`farm_add_schedules`; keeper README ops section on `zap_keeper_bot`@`main`.
- Owner of `inj1srnm…` = Dev Multisig (config.owner, after EOA→multisig rotation);
  wasm-admin = Dev Multisig. EOA signer for store/test = `inj1q2m26a…` (keyring name
  `testnet`, file backend, password in `deploy/network/mainnet.env`).
