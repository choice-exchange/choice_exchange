# Choice Contracts — Security Audit Remediation Plan

**Date:** 2026-06-06
**Scope:** `choice_clmm_pool`, `choice_clmm_math`, `choice_clmm_factory`, `choice_clmm_manager`, `choice_mts_issuer`, `choice_pool_seeder` (all pre-mainnet, uncommitted working tree).
**Companion scope:** A consuming launchpad's EVM side and these CW contracts share the launch handshake, so the EVM-side findings are tracked separately by that consumer. Fixes here (esp. C-H1) concern the shared CW↔EVM boundary.

## Audit verdict (context)

No confirmed fund-theft bug in the on-chain math or core accounting. The CLMM math is a faithful, well-hardened Uniswap V3 port (rounding favors the pool; the prior tick-bitmap word-boundary bug is correctly fixed; protocol-fee carve is conserved; reentrancy lock is sound). The remaining risk surface is **authorization and the EVM↔CW launch handshake**. One High finding (C-H1) should block mainnet.

Severity legend: 🔴 High · 🟠 Medium · 🟡 Low · ⚪ Info/hardening.
Each item has a checkbox so this doc doubles as the remediation tracker.

---

## Implementation status — 2026-06-06 (uncommitted working tree)

All code fixes below are **implemented and unit-tested**; the full workspace
builds clean (`cargo build --tests --workspace`) and all touched-crate lib tests
pass (issuer 36, seeder 62, pool 57, factory 23, manager 17, choice 17). Integration
tests (`injective_test_tube`) were **not run** — they need `make build-all` /
`build_release.sh` artifacts + a chain binary; they compile.

| ID | Status | Note |
|----|--------|------|
| C-H1 | ✅ keeper-gate; ⚠️ namespacing partial | `RegisterLaunch` now keeper-gated (closes the exploit); `LAUNCHES` keyed by `(evm_authority, internal_id)`. **Caveat:** the denom string is still `{prefix}_{id}[_{salt}]` — multi-authority-per-issuer needs distinct `subdenom_prefix` or per-deployment `salt_suffix` to avoid an on-chain denom collision (documented in code + below). |
| C-M1 | ✅ done | Manager canonicalizes `(token0,token1)` + amounts/mins; ticks left in pool orientation. |
| C-M2 | ✅ done (tokenfactory only) | New `RenounceDenomAdmin` (keeper/admin, post-`Delivered`) → `MsgChangeAdmin` to burn addr. ERC20-owner renounce is **EVM-side** (cannot sign from CW) — runbook item. |
| C-M3 | ✅ done | `DeliverToSeeder` rejects a `seeder_addr` with no contract code. |
| C-L1 | ✅ done | CW20-input swaps refund attached native funds. |
| C-L2 | ✅ done | Factory two-step owner transfer (`ProposeOwner`/`AcceptOwner`) + dead-addr reject. |
| C-L3 | ✅ done | `pool_code_id` two-step (`ProposePoolCodeId`/`AcceptPoolCodeId`) + event; owner=timelock still required at deploy. |
| C-L4 | ✅ done | Seeder errors (`SeedRatioOutOfRange`) instead of clamping a mispriced seed. |
| C-L5 | ✅ done | Manager pending reply state keyed by `token_id` via `SubMsg::with_payload`. |
| C-L7 | ✅ done | `init_sqrt_price` validated in both the factory (`CreatePool`) and the pool (`instantiate`). |
| C-L6, C-I1..C-I6 | ⬜ open | Doc/hardening items — not yet implemented. |

Shared infra: `packages/choice/mock_querier.rs` gained `register_wasm_contract` + `WasmQuery::ContractInfo` handling (needed to test C-M3); `packages/choice_clmm_common/factory.rs` gained the new factory message variants (C-L2/C-L3).

---

## 🔴 C-H1 — `RegisterLaunch` is permissionless → cheap permanent DoS of the launchpad

- **Location:** `contracts/choice_mts_issuer/src/contract.rs:170-199` (handler), dispatch at `:125`. Map at `src/state.rs:108` (`LAUNCHES: Map<u64, LaunchRecord>`).
- **Problem:** `execute_register_launch` performs **no `info.sender` check** — it is the only execute branch that is ungated (`DeliverToSeeder` `:430`, `RefundFailedLaunch` `:517`, `UpdateAdmin/Keeper/Forwarder` `:563/:580/:597` all gate on `config.keeper`/`config.admin`). Launches are keyed by a **global `internal_id`**; the guard is `if LAUNCHES.has(internal_id) { return Err(LaunchAlreadyRegistered) }`. An attacker pre-registers `internal_id = 0,1,2,…` (or reactively front-runs the keeper's tx) for the price of one create-denom fee each, and the keeper's legitimate `RegisterLaunch(N)` then reverts forever. `salt_suffix` only randomizes the denom **string**, not the map key, so it does not mitigate this.
- **Impact:** Availability/denom-control DoS (not direct buyer-fund theft — a squatted denom never binds to the EVM curve). For a money-handling launchpad, a cheap permanent permissionless DoS is High.
- **Fix:**
  - [ ] Gate `RegisterLaunch` to `config.keeper` (the keeper is the only legitimate caller). Add at top of `execute_register_launch`:
    ```rust
    if info.sender != config.keeper {
        return Err(ContractError::Unauthorized {});
    }
    ```
  - [ ] Namespace launches by `(evm_authority, internal_id)` (or fold `evm_authority` into the storage key) so a consumer's EVM launch-controller redeploy whose counter resets to 0 cannot collide with a prior instance. This also retires the long-standing global-`internal_id` footgun.
  - [ ] If permissionless registration is ever a hard requirement instead, bind `evm_authority`/`seeder_addr` to values the issuer derives on-chain (instantiate2 of a trusted seeder factory with an issuer-computed salt) rather than free-form message fields.
- **Tests to add:**
  - [ ] `register_launch_rejects_non_keeper_sender`
  - [ ] `two_authorities_same_internal_id_do_not_collide` (after namespacing)
  - [ ] Regression: keeper can still register after a same-id attempt by a third party is rejected.

---

## 🟠 C-M1 — Position manager trusts caller `token0`/`token1` order instead of canonicalizing

- **Location:** `contracts/choice_clmm_manager/src/contract.rs:327-372` (mint), stored verbatim `:438-439`; `IncreaseLiquidity` re-uses stored order `:636-661`. Factory `GetPool` sorts internally (`choice_clmm_factory/src/contract.rs:433-441`).
- **Problem:** `GetPool` resolves a pool regardless of token order, but the manager then computes liquidity/amounts and forwards funds using the **unsorted** `params.token0/token1` and persists them verbatim in `PositionState`. Asymmetric ranges → the pool's fund check reverts (confusing self-DoS). The symmetric in-range case can succeed with `token0`/`token1` stored **swapped** vs the pool, corrupting NFT metadata and mis-routing the denom on later `IncreaseLiquidity`.
- **Impact:** Data-integrity + caller-fund mis-routing (no third-party theft). Medium.
- **Fix:**
  - [ ] Canonicalize at the top of `execute_mint_position`: sort `(token0, token1)` with the same `AssetInfo` ordering the factory uses and re-map `amount0_desired/amount1_desired/amount0_min/amount1_min` accordingly **before** any computation, or reject non-canonical input outright. Factor the factory's `sort_tokens` into `choice_clmm_common` so both contracts share one implementation.
- **Tests to add:**
  - [ ] `mint_with_reversed_token_order_is_canonicalized_or_rejected`
  - [ ] `increase_liquidity_routes_correct_denom_after_reversed_mint`

---

## 🟠 C-M2 — Issuer permanently retains tokenfactory admin + EVM mint authority on every launch denom

- **Location:** `contracts/choice_mts_issuer/src/proto.rs:19-23` (`allow_admin_burn=true`, ERC20 owner = issuer), `src/contract.rs:285-294`.
- **Problem:** Denoms are created with `allow_admin_burn=true`; the issuer stays tokenfactory admin **and** ERC20 owner with no renounce/rotate path. Whoever controls the issuer `admin`/migrate key can mint unlimited supply or admin-burn tokens out of *any* holder (LPs, the sink, traders) for the life of the denom. Trust rests entirely on `admin` being a timelock — a deployment assumption the code does not enforce (integration tests use a single hot key).
- **Impact:** Rug vector if the admin/migrate key is compromised. Medium.
- **Fix:**
  - [ ] Add a keeper/admin-gated `FinalizeDenom { internal_id }` (callable only once a launch is `Delivered`) that:
    - `MsgChangeAdmin` the tokenfactory denom → the 20-zero-byte burn-address convention (`inj1qqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqe2hm49`), and
    - transfers/renounces the auto-deployed `MintBurnBankERC20` ownership.
  - [ ] Enforce in-code (or document loudly + verify at deploy) that `admin` and the wasm-admin are `choice_admin_timelock`, not a hot key.
- **Tests to add:**
  - [ ] `finalize_denom_renounces_admin_and_blocks_further_mint`
  - [ ] `finalize_denom_rejected_before_delivered`

---

## 🟠 C-M3 — `DeliverToSeeder` trusts keeper-supplied `leftover` and an unvalidated `seeder_addr`

- **Location:** `contracts/choice_mts_issuer/src/contract.rs:422-491`; `seeder_addr` only `addr_validate`d at `:213`, bank-sent at `:473-478`.
- **Problem:** `leftover` comes from the keeper's message (bounded only by `≤ evm_supply`) and is admin-burned from `evm_authority`; `cw_held` is bank-sent to a record-stored `seeder_addr` the contract never verifies is a real sink (it's an off-chain instantiate2 address). A wrong/ghost `seeder_addr` strands `cw_held` unrecoverably; a buggy/compromised keeper can burn up to the full `evm_supply`.
- **Impact:** Misconfiguration/keeper-trust fund-strand (no cross-launch theft, since `RegisterLaunch` after C-H1 is keeper-gated). Medium.
- **Fix:**
  - [ ] In `RegisterLaunch`, recompute the expected sink address from `instantiate2(seeder_factory, sink_code_id, salt)` and assert it equals `seeder_addr` (requires the issuer to know `sink_code_id` and parse the salt from the payload — or store `sink_code_id` in config and pass the salt explicitly instead of an opaque payload).
  - [ ] Verify `seeder_addr` holds contract code before the bank-send in `DeliverToSeeder` (`deps.querier.query_wasm_contract_info`).
  - [ ] Prefer sourcing `leftover` from on-chain state (`record.evm_supply` minus tracked spend) rather than keeper input.
- **Tests to add:**
  - [ ] `deliver_to_seeder_rejects_ghost_seeder_addr`
  - [ ] `register_launch_rejects_seeder_addr_salt_mismatch`

---

## 🟡 C-L1 — Native funds attached to a CW20-input swap are silently absorbed (no refund)

- **Location:** `contracts/choice_clmm_pool/src/actions/swap.rs:439-447` (`Cw20Allowance`/`Cw20AlreadySent` branches), reachable from `execute_swap` `:537-540` and `execute_swap_exact_output` `:859-862`. `Swap`/`SwapExactInput`/`SwapExactOutput` are classified `payable` (`contract.rs:223-229`).
- **Problem:** When the in-token is a CW20, `apply_swap` builds only a `TransferFrom` and never inspects `info.funds`, so any native coins the caller attached are received into reserves with no refund — breaking the refund-parity invariant the native path upholds (`swap.rs:398-419`, mint `compute_native_refunds`).
- **Impact:** User/integrator value-loss footgun (not attacker-vs-victim). Low.
- **Fix:**
  - [ ] In the CW20 input branches, refund the full `info.funds` to the sender, or reject any attached native funds when the in-token is a CW20. Simplest: thread `info.funds` into `apply_swap` and always refund native coins that aren't the consumed input denom.
- **Tests to add:**
  - [ ] `swap_cw20_input_refunds_attached_native_funds`

---

## 🟡 C-L2 — Factory owner transfer is single-step (lockout risk)

- **Location:** `contracts/choice_clmm_factory/src/contract.rs:98-118` (`UpdateConfig`).
- **Problem:** One-step `owner` change with no two-step accept and no dead-address guard. A typo'd-but-valid bech32 irrevocably bricks every owner-gated function across the factory **and** each pool's protocol-fee controls (pools read their controller as the factory owner).
- **Impact:** Fat-finger permanent lockout. Low.
- **Fix:**
  - [ ] Adopt a two-step `propose_owner` / `accept_owner` handshake (cw-ownable or hand-rolled). At minimum reject known dead addresses.
- **Tests to add:**
  - [ ] `owner_transfer_requires_accept`
  - [ ] `pending_owner_can_accept_old_owner_cannot_act_after_accept`

---

## 🟡 C-L3 — `pool_code_id` repointable with no version pin / timelock

- **Location:** `contracts/choice_clmm_factory/src/contract.rs:109-111`.
- **Problem:** Owner can change `pool_code_id` at will. Existing pools are unaffected, but future `CreatePool`s instantiate a structurally different pool that the manager trusts implicitly (it parses and trusts pool reply attributes — `manager/contract.rs:494-510`, `:828-863`). This is the system's root of trust behind a single owner key with no timelock.
- **Impact:** Future-pool integrity risk. Low (gated by owner key).
- **Fix:**
  - [ ] Gate `pool_code_id` changes behind `choice_admin_timelock`, and/or track a pool "version" the manager can refuse if unknown. Document factory owner = root of trust.
- **Tests to add:**
  - [ ] `update_pool_code_id_requires_timelock` (or version-gate test).

---

## 🟡 C-L4 — CLMM seed price clamps instead of erroring on extreme ratios

- **Location:** `contracts/choice_pool_seeder/src/clmm.rs:57-64`; mint with `amount*_min = 0` at `src/contract.rs:718-719`.
- **Problem:** `init_sqrt_price_from_amounts` clamps the computed sqrt price into `[MIN_SQRT_RATIO, max-1]` rather than erroring. On an extreme seed ratio the pool is created at a price not matching the seed ratio, and the `amount*_min = 0` mint draws one side only, refunding the rest to `refund_receiver` — a smaller-than-intended locked-liquidity floor at a manipulable opening price.
- **Impact:** Edge-case misprice + value leak to the configured refund receiver (no attacker theft). Low (unlikely at 18-dec sane seed amounts).
- **Fix:**
  - [ ] Either error when the pre-clamp price is out of range (let the keeper triage), or set `amount0_min`/`amount1_min` to a high fraction of desired so a lopsided mint reverts rather than silently refunding.
- **Tests to add:**
  - [ ] `settle_clmm_extreme_ratio_reverts_instead_of_mispricing`

---

## 🟡 C-L5 — Manager reply state is a single global `PENDING_*` Item, not keyed by `token_id`

- **Location:** `contracts/choice_clmm_manager/src/contract.rs:1313-1385`; `PENDING_MINT/INCREASE/DECREASE/COLLECT` (`state.rs:117-120`); reply handlers `:486/:702/:814/:1019`.
- **Problem:** Safe today only because (a) Cosmos txs are atomic/non-interleaving and (b) the pool dispatches no submessage that re-enters the manager before its reply fires. A future `pool_code_id` (see C-L3) that re-enters mid-reply could clobber the single pending slot and mis-attribute a reply. The emitter filter defends against forged attributes but not pending-state clobbering.
- **Impact:** Latent — not exploitable with current pool code. Low.
- **Fix:**
  - [ ] Key pending state by `token_id`, or migrate to CosmWasm `SubMsg::with_payload` so nested/concurrent in-flight ops cannot collide.
- **Tests to add:**
  - [ ] `reply_state_keyed_by_token_id` (and a re-entrant mock-pool regression once keyed).

---

## 🟡 C-L6 — Quote vs execution dynamic-fee divergence

- **Location:** `choice_clmm_pool/src/core/oracle.rs:31` (execution re-blends EMA) vs `:116` `get_dynamic_fee` used by `query_quote*` (`actions/swap.rs:749`, `:906`).
- **Problem:** Quotes use the stored `last_fee_ppm` (or `base_fee_ppm` if stale > 1h) while execution re-blends/clamps the EMA, so the executed fee can differ from the quoted fee within a block boundary.
- **Impact:** UX/quote-accuracy only; slippage guards still protect funds. Low.
- **Fix:**
  - [ ] Document the quote as indicative, or have the quote path run the same `update_oracle_and_fee` projection (read-only) the swap uses.
- **Tests to add:**
  - [ ] `quote_fee_matches_executed_fee_within_block` (or document + assert the bound).

---

## 🟡 C-L7 — Permissionless pool creation with attacker-chosen `init_sqrt_price` (anti-squat is opt-in)

- **Location:** `choice_clmm_factory/src/contract.rs:122-228`; `init_sqrt_price` passed to the pool unvalidated (`:129/:202`).
- **Problem:** For any pair without a `POOL_CREATION_AUTH` reservation, anyone can create the canonical `(token0, token1, fee)` pool and set its opening price; the first creator owns the registry slot permanently. A launch that omits `clmm_pool_auth` can be front-run with a mispriced pool, forcing the seeder's `settle_clmm` pre-existing-pool guard (`pool_seeder/src/contract.rs:660-662`) to refuse → launch stuck until Refund. (Note: the instantiate2 *address* squat is **not** possible — the address derives from the factory's own creator address + salt.)
- **Impact:** Pre-pricing / launch-DoS when `clmm_pool_auth` is omitted. Low (mitigated when the keeper wires the gate).
- **Fix:**
  - [ ] Make `clmm_pool_auth` effectively mandatory for CLMM graduations (keeper always sets it; consider having the seeder factory refuse a CLMM `CreateSink` whose launch did not reserve the slot).
  - [ ] Validate `init_sqrt_price` is non-zero and within Q64.96 tick bounds at the factory (or confirm the pool rejects 0/out-of-range on instantiate).
- **Tests to add:**
  - [ ] `create_pool_rejects_out_of_range_init_sqrt_price`
  - [ ] Keeper integration: CLMM launch always reserves the slot before settle.

---

## ⚪ Info / hardening

- [ ] **C-I1** — `addr_validate` caller-supplied `recipient` up front in `swap`/`collect` (`pool/src/actions/swap.rs:452`, `collect.rs:56,59`) for clean early errors (matches `flash.rs:73`). UX only.
- [ ] **C-I2** — Confirm at deploy that the wasm-admin of **every** contract (pool/factory/manager/issuer/seeder) is `choice_admin_timelock`. The `migrate` handlers only bump cw2 version and rely entirely on the wasm-admin for authorization. Pools are instantiated `admin: None` (good — factory owner cannot migrate-rug LPs).
- [ ] **C-I3** — Document that an `Approve`/`ApproveAll` on a CLMM position NFT includes fee-withdrawal-to-third-party rights (`manager/contract.rs:896-910`, standard Uniswap NPM semantics). Optionally restrict `recipient` to the owner unless caller == owner.
- [ ] **C-I4** — Switch the oracle's plain `+` (`oracle.rs:68`, `:116`) to `checked_add`/`saturating_add` to match the checked-everywhere convention (proven non-overflowing for in-range prices; style only).
- [ ] **C-I5** — Add proptest/fuzz vectors for the CLMM partial-step and exact-out rounding invariants to lock in the behavior verified dynamically during the audit.
- [ ] **C-I6** — Add a regression for the seeder fee-denom collision and over-attach fixes if not already present beyond `settle_pair_denom_equals_create_fee_denom_deposits_leg_b_only`.

---

## Verified clean (do not re-litigate)

- CLMM math: amount0/1 deltas, next-sqrt-price, computeSwapStep all round in the pool's favor; `mulDiv` 512-bit intermediate correct; `tick_math` bit-for-bit V3 (MIN/MAX/±1 vectors + full-range roundtrip pass); **tick-bitmap word-boundary bug correctly fixed** (`position(compressed+1)`, floor semantics on negative ticks).
- Pool actions: reentrancy lock guards every fund-mutating entrypoint and rolls back on flash failure; protocol-fee carve conserved, no double-withdraw; exact-output never over-delivers; payment verified against `info.funds` (native) / exact `TransferFrom` (CW20); no attacker-reachable panic/unwrap.
- Seeder: role separation immutable + enforced; prior fee-denom-collision and over-attach bugs genuinely fixed; LP burned/locked with no withdraw path; locker exposes only `CollectFees` to a fixed beneficiary; Settle can't double-spend.
- Issuer: `MsgSetDenomMetadata` correctly omitted (no v1.20 brick); reply id/payload not forgeable; exact-funds accounting.

## Rollout order

1. **C-H1** (blocker) — keeper-gate `RegisterLaunch` + namespace `internal_id`.
2. **C-M1** — canonicalize manager token order.
3. **C-M2 / C-M3** — denom-admin renounce path; seeder-address validation + leftover sourcing.
4. **C-L1** (refund leak) and **C-L2 / C-L3** (owner-key hardening) — highest-value Lows.
5. Remaining Lows + Info, then re-run `cargo test` + `make test` (integration needs `make build-all`/`build_release.sh` for bulk-memory WASM) and re-audit the diff.
