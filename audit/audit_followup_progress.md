# Audit follow-up — vault + farm, progress handoff

Session context: landed security-audit fixes for `choice_vault` (primary focus) and `choice_farm`. Full unit + integration test coverage for every batch.

**All file references are relative to `choice_exchange/`.**

## State of the tree

- `contracts/choice_vault/`: Batches 1–4, 6 below landed. `cargo test -p choice-vault --lib` → 95/95 pass. Clippy clean (2 pre-existing deprecation warnings on `SubMsgResponse::data` in unrelated test code).
- `contracts/choice_farm/`: unchanged this session. Recommendations in H-4 noted but not implemented — they'd change a contract that other consumers may depend on and deserve their own scoped PR.
- Schema regenerated via `cargo run --example vault_schema` (run from `contracts/choice_vault/`).

## What landed

### Batch 1 — instantiate & config hygiene

| Item | Where | Note |
|---|---|---|
| M-4 | `contracts/choice_vault/src/contract.rs:74-80` | Already in tree on arrival; route-terminates-on-pair-asset invariant. |
| M-5 | `contracts/choice_vault/src/contract.rs` — `MAX_SLIPPAGE_TOLERANCE` const | Hardcoded to **25%** after user pushback on 10%. See "Design questions" below — this is a conscious tradeoff, not the final design. |
| M-6 | `contracts/choice_vault/src/contract.rs:836-840` | Replaced `multiply_ratio(atomics, 10^18)` with `.mul_floor(Decimal)`. |
| M-7 | `ExecuteMsg::ClearFeeRecipient` | New owner-only message; `UpdateConfig` still can't unset, only set. |

### Batch 2 — deposit/withdraw hygiene

| Item | Change |
|---|---|
| M-1 | `execute_deposit_native` requires exactly one coin of the LP denom (was: find + silently retain extras). |
| M-8 | Dead `total_shares.is_zero()` branch in `execute_withdraw_shares` → explicit invariant-violation error. |
| L-15 | `execute_activate_my_deposit` auto-refunds dust (unbond + transfer). Batch path (`execute_activate_pending_deposits`) still skips with a `skipped_dust_count` attribute — adversarial dust-spam would bloat gas otherwise; affected users self-rescue via `ActivateMyDeposit`. |

### Batch 3 — compound flow & MEV

| Item | Change |
|---|---|
| C-3 (remaining leg) | `Compound` is now **permissionless** (no compounder gate). `minimum_lp_to_receive` is now a required non-zero `Uint128` (was `Option<Uint128>`). `compounder` field retained only for `ActivatePendingDeposits` gate and rotation ergonomics. |
| H-3 | `optimal_zap_amount_xyk` helper: closed-form solution of the zap-in quadratic for a 0.3%-fee XYK pair. Replaced both 50/50 split sites. Queries pair `Pool {}` for reserves. **CLMM explicitly deferred** — helper's doc comment flags where the branch belongs. |
| M-2 | `handle_harvest_reply` now records the actual post-harvest, post-fee reward balance into the payload — was recording `staker_info.pending_reward` pre-Withdraw prediction. |
| M-3 | Harvest submsg switched to `ReplyOn::Always`; handler surfaces a structured step-1 error before reverting. Swap-level submsgs kept on `Success` — changing them would require per-hop partial-state reasoning that's out of scope and dangerous. Honest caveat: this is mostly cosmetic since the tx still reverts either way. |

### Batch 4 — operational safety

| Item | Change |
|---|---|
| H-4 | `paused: bool` on Config (`#[serde(default)]`, no migration needed). `ExecuteMsg::Pause` / `Unpause` (owner-only). `assert_not_paused` guard on `receive_cw20`, `execute_deposit_native`, `execute_compound`, `execute_activate_pending_deposits`, `execute_activate_my_deposit`. Withdraw paths deliberately unguarded. |
| L-11 | `migrate` entry point pattern mirrored from `choice_farm`: contract-name check + cw2 version bump. `MigrateMsg {}` struct. |
| L-13 | **Deferred** — see "Deferred work" below. |

## L-13 integration tests — scaffold landed

`tests/vault_integration.rs` is wired into `packages/choice_clmm_common/Cargo.toml` as a `[[test]]` target (same pattern as `integration.rs`). Currently **7/7 passing**:

| # | Test | Covers |
|---|---|---|
| 1 | `native_native_deposit_activate_compound_withdraw` | end-to-end happy path, empty route, permissionless C-3 compound with gain |
| 2 | `pause_blocks_entry_but_allows_exit` | H-4 — Deposit/Activate*/Compound rejected while paused; `WithdrawShares` unaffected; Unpause restores entry |
| 3 | `compound_reverts_on_tight_belief_price` | partial-state safety — bad belief trips `MaxSpreadAssertion`, `total_shares` + farm `bond_amount` unchanged, retry works |
| 4 | `activate_my_deposit_refunds_dust` | L-15 auto-refund when share price rounds pending to zero |
| 5 | `migrate_staking_does_not_strand_vault_users` | `MigrateStaking` forwards only `undistributed_rewards`; vault bonds + credited rewards survive; `WithdrawShares` still distributes the pre-migration reward slice |
| 6 | `native_cw20_deposit_activate_compound_withdraw` | `atom/CW20` pair — exercises vault's `IncreaseAllowance` + CW20 leg in `ProvideLiquidity` and receiving CW20 as swap return |
| 7 | `cw20_cw20_deposit_activate_compound_withdraw` | both pair assets CW20, CW20 reward — exercises CW20 swap path (`cw20.Send(pair, …Cw20HookMsg::Swap)`), double-CW20 allowance in ProvideLiquidity, CW20 reward harvest via `cw20.Transfer` |

Run with `cargo test --test vault_integration` from `choice_exchange/`. **Always run `./build_release.sh` first** — `make build-all` only covers CLMM artifacts, and integration tests rely on the full set (vault/farm/legacy pair/factory/router/send_to_auction).

### Things that tripped the scaffold and are worth remembering

- **`burn_address` must be a real contract.** Pair sends swap-fee burns via `BurnAuctionExecuteMsg::SendNative`. An EOA → "no such contract" inside the reply chain. Setup deploys `choice_send_to_auction` (subaccount `0x1111…1111`) and passes its address as factory `burn_address`.
- **CW20 swap fees require a real `cw20_adapter` contract.** When the swap return is a CW20, the pair routes the burn fee to `auction.Receive(cw20)` → auction does `cw20.Send(adapter, amount)` → adapter mints a factory denom and deposits it to the exchange burn subaccount. An EOA adapter fails with "no such contract" because CW20 `Send` calls the recipient's hook. Also: the adapter needs INJ on hand to pay the token-factory denom-create fee (one per distinct CW20 it sees). Setup deploys `cw20_adapter.wasm` and bank-sends 100 INJ to it.
- **Farm distribution must start *after* the first Activate.** `execute_deposit_native` bonds immediately, so any live schedule accrues rewards before the first activation runs → `PendingRewardsMustBeCompounded` trips on the dilution guard. Setup delays `schedule_start` by 60s (120s for the CW20/CW20 scenario with heavier setup).
- **Auto gas estimation undershoots `WithdrawShares`' reply chain.** Every signer gets `FeeSetting::Custom { gas_limit: 50_000_000 }`. Without this, simulation-based gas is ~417k and the reply chain OOMs mid-harvest.
- **`WithdrawShares` returns LP, not underlying assets.** Users get a proportional slice of the vault's staked LP (compound gain included); unwrapping to atom/usdt is a separate step the user opts into.
- **Pair `belief_price` is offer/ask, not ask/offer.** `expected_return = offer_amount / belief_price`. To force a MaxSpreadAssertion in tests, set belief *low* (0.01 implies "I expect 100× return"), not high.
- **`query_staker_info` needs `block_time: Some(now)` to project accrued-but-uncredited rewards.** With `None`, you only see what was credited on the last bond/unbond — easy source of false-negative assertions in tests.
- **Pair LP is always native (`factory/{pair}/lp`).** Users without any CW20 holdings can still deposit into a CW20/CW20 vault — admin seeds the pair, then bank-sends LP directly to the user. The vault's CW20 receive hook (`Receive(Cw20ReceiveMsg)` → `Cw20HookMsg::Deposit`) is effectively dead code in this system; only the compound-side CW20 paths are exercised.

### Batch 6 — vault governance (min_lp heuristic + timelocked slippage cap)

Landed 2026-04-19. Closes out the non-audit backlog. `contracts/choice_vault/` → 95/95 unit tests pass; `contracts/choice_farm/` → 21/21; `tests/vault_integration.rs` → 8/8 (added one scenario). Schema regenerated via `cargo run --example vault_schema`.

| Item | Where | Note |
|---|---|---|
| `minimum_lp_to_receive` heuristic | `contracts/choice_vault/src/contract.rs` — `estimate_expected_lp`, `MIN_LP_HEURISTIC_K` const, check in `execute_compound` | Walks `reward_to_lp_token_route` via pair `Simulation` queries (decimals-aware), projects LP via closed-form XYK on `pair_contract`. Rejects `min_lp < 10% * expected_lp` with `MinimumLpBelowHeuristic`. Fails open when `expected_lp * k` rounds to zero — big-pool dust compounds keep working. |
| `MAX_SLIPPAGE_TOLERANCE` → `Config.max_slippage_tolerance` | `state.rs`, `msg.rs`, `contract.rs` — `TightenMaxSlippage` / `ProposeMaxSlippageRaise` / `ApplyMaxSlippageRaise` / `CancelMaxSlippageProposal` | Instant tighten (requires `new_max <= current`), 48h timelocked raise (requires `new_max > current` and `<= MAX_SLIPPAGE_TOLERANCE_CEILING = 50%`). Apply revalidates the ceiling in case the const is lowered while a raise is in flight. Tighten also clamps `slippage_tolerance` when it would otherwise exceed the new cap. |
| Observability | `QueryMsg::PendingMaxSlippageRaise` + `PendingMaxSlippageRaiseResponse` | Matches the farm Batch 5 pattern — stakers can see pending raises to time their exit. |
| Const rename | `MAX_SLIPPAGE_TOLERANCE` → `DEFAULT_MAX_SLIPPAGE_TOLERANCE` | Semantics shifted from "the cap" (static) to "the initial cap" (per-vault via config). Also added `MAX_SLIPPAGE_TOLERANCE_CEILING = 50%` (hard ceiling) and `MAX_SLIPPAGE_RAISE_DELAY_SECONDS = 48h`. |
| Breaking API: `Config` fields | `state.rs` | Added `max_slippage_tolerance`, `pending_max_slippage`, `pending_max_slippage_effective_at` — all `#[serde(default)]` so pre-governance stored configs deserialize as `25% / None / None` without a storage migration. |

#### Design notes — `k = 10%`

Chosen so the heuristic never false-positives a legitimate worst-case caller. Worst-case shortfall is `(1 - max_slippage_tolerance)^(hops)`. At the ceiling (50%) over a 2-hop route (3 swaps), that's `0.5^3 = 12.5%` of expected LP — k = 10% admits this while still forcing any caller to commit to >10% of fair LP. Not a tight MEV bound (individual `belief_prices` + `assert_max_spread` remain primary); the heuristic is a sanity floor that catches `Uint128::new(1)`-style blatantly-low values that would make the C-3 permissionless-compound safeguard vacuous.

#### Integration test update

`tests/vault_integration.rs` existing 7 tests: every `Compound { minimum_lp_to_receive: Uint128::new(1) }` bumped to `Uint128::new(100_000_000)` to clear the heuristic floor (~7e7 at the 10^11-reserve pool scale used in the setup). One test added: `compound_heuristic_rejects_min_lp_of_one` — end-to-end verification that the on-chain `estimate_expected_lp` math matches real pair `Pool {}` responses, rejecting min_lp = 1 with `MinimumLpBelowHeuristic`. Guards against math drift between the unit-test mocks and the pair's real responses.

### Batch 5 — farm-side hardening (H-4 follow-through)

Landed 2026-04-19 earlier this session. Vault-side code untouched. `contracts/choice_farm/` → 21/21 unit tests pass; `tests/vault_integration.rs` → 7/7 pass after updating the migration flow.

| Item | Where | Note |
|---|---|---|
| Farm owner rotation | `contracts/choice_farm/src/contract.rs` — `propose_new_owner`, `apply_owner_rotation`, `cancel_owner_proposal` | 48h propose→apply→cancel, modeled on vault's C-3 compounder rotation. Old owner keeps full rights until the apply fires; the 48h window is the user exit window, not a second-party-accept step. |
| Timelocked `migrate_staking` | `contracts/choice_farm/src/contract.rs` — `propose_migrate_staking`, `apply_migrate_staking`, `cancel_migrate_staking_proposal` | Replaced the instant `MigrateStaking` message (now removed from `ExecuteMsg`). `apply_migrate_staking` calls `compute_reward` then forwards `undistributed_rewards`; bonded stake and credited pending rewards are untouched. |
| Observability | `QueryMsg::PendingOwnerRotation`, `QueryMsg::PendingMigration` + new `PendingOwnerRotationResponse` / `PendingMigrationResponse` | Users need to see pending proposals to know when to exit. |
| `ConfigResponse.owner` | `packages/choice/src/staking.rs` + `query_config` | The old `ConfigResponse` hid the owner field — added so stakers can verify who owns the contract they're bonded to. |
| Deployment hygiene note | `contracts/choice_farm/src/contract.rs` — doc comment above `instantiate` | Flags that the wasm `admin` address must be a timelocked multisig, since `MigrateContract` bypasses the in-contract timelock on migrate_staking. |

### Breaking API changes (farm)

- `ExecuteMsg::MigrateStaking { new_staking_contract }` **removed**. Replaced by the propose/apply/cancel trio. Any off-chain tooling that called it directly must switch to the two-step flow.
- `ConfigResponse` gained an `owner: String` field. Existing consumers that construct `ConfigResponse` literals or parse via exact-match must update.
- `Config` (storage) gained `pending_owner: Option<CanonicalAddr>` and `pending_owner_effective_at: Option<u64>`, both `#[serde(default)]`, so in-place `cw2`-style migration reads pre-hardening instances as "no pending rotation" without a storage migration.
- New storage item `PENDING_MIGRATION: Item<PendingMigration>` (absent by default — `may_load` returns `None`).

### What wasn't done (intentional)

- **`update_config` is not timelocked.** The audit callout flagged both `migrate_staking` and `update_config`, but `update_config` is already narrowly constrained by `assert_new_schedules` (owner can only add future slots, never remove or back-date). The worst-case with `update_config` is delayed distribution, not theft. Out-of-scope for this batch; revisit if the constraint changes.
- **New-owner accept step.** The template used (compounder rotation) does not require the incoming owner to accept. Operators must verify the target address themselves before proposing. If a two-party accept is later wanted, the vault's `ProposeNewOwner` / `AcceptOwnership` pattern can be composed in — but given the user guidance to wire the owner to a timelocked multisig, this is belt-and-suspenders.

### Integration test update

`tests/vault_integration.rs` — `migrate_staking_does_not_strand_vault_users` was refitted to the propose/apply flow. It now asserts:
- premature apply rejects,
- owner applies after 48h+1s,
- vault bond + pre-migration credited rewards survive the migration.

## Deferred work (for the new session)

### 1. H-3 CLMM support — **the only remaining item; blocked on interop plan**

The helper in `contract.rs` (`optimal_zap_amount_xyk`, `estimate_expected_lp`) and the `query_pair_offer_reserve` site are XYK-only. When the vault starts backing CLMM pools, needed:

- A pair-type discriminator (either at `instantiate` via a new config field, or inferred from the pair's `Pair {}` query response shape).
- A CLMM zap routine — the closed-form XYK derivation doesn't hold because liquidity is tick-local. Likely path: use current `sqrt_price` from the CLMM pool, compute 50/50 in price terms, rely on per-call `min_out` to bound error. Probably needs a separate submsg path too since CLMM adds liquidity via the manager, not `ProvideLiquidity`.
- Update `create_swap_submsg` (or replace it) — CLMM swaps go through `choice_clmm_pool` / `choice_clmm_manager`, not `choice_pair`.
- Update `estimate_expected_lp` accordingly — Simulation queries and the LP formula both need CLMM variants.

This is a **feature extension**, not a bugfix — the vault today only supports XYK pairs. **Do not start until there is a concrete plan for vault↔CLMM interop** (tick ranges, NFT position management). Flagging as such: the audit work is formally "done" with Batch 6; H-3 CLMM is a product decision, not a pending audit finding.

### 2. L-13 integration tests — remaining scenarios

All 7 planned scenarios are in place. New build-system pieces that landed along the way:

- `contracts/cw20_base_build/` — thin wrapper crate that re-exports `cw20-base` entry points so the workspace-optimizer produces `artifacts/cw20_base_build.wasm`. Only used by integration tests. Not deployed.
- `artifacts/cw20_adapter.wasm` — copied from `cw20_adapter/cw20_adapter.wasm` in the repo. The upstream build isn't ours to rerun, so `build_release.sh` does not produce it; keep the copy fresh if the upstream changes.

Possible extensions when appetite returns:

- Multi-user interleaving (two depositors compound between each other's activations) to exercise share-price fairness under concurrent activity.
- Compound while `route.len() > 0` — a reward token that must route through a second pair to reach a pair asset. Covers the route-swap reply chain that's currently only exercised in unit tests.

### 3. Farm-side hardening (H-4 follow-through) — **LANDED in Batch 5**

See the Batch 5 section above. Summary:

- Farm owner is now rotatable via a 48h propose→apply→cancel flow.
- `migrate_staking` is now timelocked on the same 48h pattern; the old instant `MigrateStaking` message was removed.
- Deployment-hygiene comment added to the farm contract flagging the wasm-admin requirement.
- `update_config` timelock was evaluated and left out — see "What wasn't done" in Batch 5 for the reasoning.

## Design questions to resolve with user

### MAX_SLIPPAGE_TOLERANCE is a const at 25% — **RESOLVED in Batch 6**

Originally landed as a const at 25% with the caveat that a meaningful tunable cap needed the governance split. Batch 6 landed that split: `Config.max_slippage_tolerance` is now mutable, with instant tighten (`TightenMaxSlippage`) and 48h timelocked raise (`ProposeMaxSlippageRaise` / `ApplyMaxSlippageRaise` / `CancelMaxSlippageProposal`). Hard ceiling of 50% (`MAX_SLIPPAGE_TOLERANCE_CEILING`) still blocks the timelocked raise from landing on nonsensical values — the 48h window alone can't compensate for a cap that's been bumped to, say, 95%.

### `minimum_lp_to_receive` can be set to 1 by a caller — **RESOLVED in Batch 6**

Heuristic landed: `execute_compound` calls `estimate_expected_lp` (simulate the reward route via pair `Simulation`, closed-form LP on pair_contract) and requires `min_lp >= 10% * expected_lp`. Oracle-based bounds remain deferred — no Injective price oracle is wired up today, and the heuristic already catches the blatant-misconfiguration class (`Uint128::new(1)`, etc.) without one. Honest posture: this is a sanity floor, not a tight MEV bound — per-swap `belief_prices` + `assert_max_spread` remain the primary MEV defense, as noted in `MIN_LP_HEURISTIC_K`'s doc comment.

### M-3 is mostly cosmetic

Switching harvest to `ReplyOn::Always` doesn't change observable behavior — tx still reverts. Only benefit is the structured error attribute in tx logs. If the user wants to revisit (e.g., soft-fail compound that leaves rewards in-vault for retry), that's a different design with partial-state risks.

## Resuming in a new session — concrete starting points

1. Read this file and `audit/audit_fixes_v0.1.md` (prior audit-fix context, not mine).
2. Key code paths to re-orient:
   - `contracts/choice_vault/src/contract.rs` — vault batches 1–4, 6 touch this file. Key adds from Batch 6: `estimate_expected_lp`, `compute_reward_after_fee`, `MIN_LP_HEURISTIC_K`, the 4 slippage-governance handlers.
   - `contracts/choice_vault/src/msg.rs` — entry message shapes (note `minimum_lp_to_receive` is `Uint128`, not `Option`). Batch 6 added `TightenMaxSlippage` / `ProposeMaxSlippageRaise` / `ApplyMaxSlippageRaise` / `CancelMaxSlippageProposal` and `QueryMsg::PendingMaxSlippageRaise`.
   - `contracts/choice_vault/src/state.rs` — `paused` field with `#[serde(default)]`. Batch 6 added `max_slippage_tolerance`, `pending_max_slippage`, `pending_max_slippage_effective_at` (all `#[serde(default)]`), plus the `MAX_SLIPPAGE_RAISE_DELAY_SECONDS` const.
   - `contracts/choice_vault/src/mock_querier.rs` — `with_pool` helper added for H-3, `with_simulation` added for the B-6 heuristic (Simulation is keyed on json-serialized AssetInfo since it lacks `Hash`).
   - `contracts/choice_farm/src/contract.rs` — Batch 5 lives here. Timelocked `propose_migrate_staking` / `apply_migrate_staking` / `cancel_migrate_staking_proposal` + owner rotation trio.
   - `contracts/choice_farm/src/state.rs` — `Config.pending_owner`, `Config.pending_owner_effective_at` (both `#[serde(default)]`), new `PENDING_MIGRATION` item, `TIMELOCK_DELAY_SECONDS` const.
   - `packages/choice/src/staking.rs` — shared ExecuteMsg. Old `MigrateStaking` variant is **gone**; any external caller must use the propose/apply flow.
3. Sanity tests:
   - `cargo test -p choice-vault --lib` → 95/95
   - `cargo test -p choice-farm --lib` → 21/21
   - `cargo test -p choice-clmm-common --test vault_integration` → 8/8 (after `./build_release.sh`)
4. Remaining workstreams:
   - **H-3 CLMM zap** is the only open item. Feature extension, not a bugfix — blocked on a concrete vault↔CLMM interop plan (tick ranges, NFT position management). With Batch 6 landing, the original audit is formally closed; anything beyond H-3 is new product work.

## Audit findings close-out status

From the original summary in the opening message of this session:

- Critical: C-1 ✓ · C-2 ✓ · C-3 ✓
- High: H-1 ✓ · H-2 ✓ · H-3 ✓ (XYK only) · H-4 ✓ (farm-side hardening landed Batch 5 2026-04-19) · H-5 ✓
- Medium: M-1 ✓ · M-2 ✓ · M-3 ✓ (cosmetic) · M-4 ✓ · M-5 ✓ · M-6 ✓ · M-7 ✓ · M-8 ✓
- Low: L-11 ✓ · L-13 ✓ (7/7 scenarios landed Batch 4; extensions in "Deferred work" are nice-to-haves) · L-15 ✓

Remaining items on the shared backlog (not original-audit):

- ~~`minimum_lp_to_receive` heuristic bound~~ — landed Batch 6 (2026-04-19)
- ~~`MAX_SLIPPAGE_TOLERANCE` timelocked raise~~ — landed Batch 6 (2026-04-19)
- H-3 CLMM zap (feature extension, needs vault↔CLMM interop plan first)

**Audit is formally closed** — every original finding is resolved; the one remaining item (H-3 CLMM) is a feature extension the vault does not need in its XYK-only form.

## Notes for future me

- `Uint256::isqrt()` requires `use cosmwasm_std::Isqrt` (2018 edition — no prelude).
- `Uint128::try_from(Uint256)` requires `use std::convert::TryFrom` (same reason).
- The vault's `create_swap_submsg` still uses `ReplyOn::Success` for both swap legs. Don't blindly change to `Always` without thinking through partial-state scenarios in the reply chain.
- Test helper `big_pool_response` in `testing.rs` uses `10^12` reserves, which makes `optimal_zap_amount_xyk` round to essentially 50/50 — that's why pre-H-3 assertions survived the formula swap without math churn. If you want to test H-3 behavior specifically, use smaller reserves (see `test_optimal_zap_amount_matches_fee_derivation`).
- Same helper makes the B-6 heuristic fail open at small reward scales (`expected_lp * 10%` floors to zero vs huge reserves). New tests that need to exercise the heuristic's non-zero floor use a tighter `PoolResponse` via `setup_heuristic_vault` — both reserves + total_share at 10_000, `pending_reward = 100` → concrete floor of 5.
- `AssetInfo` doesn't implement `Hash`, so the mock querier keys `Simulation` responses on `(pair_addr, to_json_string(info))`. If a future refactor adds `Hash` to `AssetInfo`, the map key can become `(String, AssetInfo)` directly.
- On the slippage governance: `TightenMaxSlippage` intentionally does **not** clear a pending raise proposal. If the owner tightens and wants the raise blocked, they must `CancelMaxSlippageProposal` explicitly — otherwise the raise fires after 48h and restores whatever `pending_max_slippage` holds (`ApplyMaxSlippageRaise` re-checks the ceiling before applying, but not against the post-tighten cap). This is intentional: the 48h window is the user exit guarantee, and silently cancelling pending raises would let the owner sidestep that guarantee by briefly tightening.
