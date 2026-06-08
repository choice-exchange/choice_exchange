# Choice CLMM — Adversarial Edge-Case Fuzzing Report

External security review, focused on *breaking* the Choice CLMM contracts:
`choice_clmm_pool`, `choice_clmm_factory`, `choice_clmm_manager`,
`packages/choice_clmm_math`, `packages/choice_clmm_common`.

**Result: no fund-safety or accounting-invariant violation was found** on the
covered surface, across ~675k randomized op-sequences plus 23 hand-built extreme
scenarios. Every new invariant was proven non-vacuous by mutation testing. One
*pre-existing test-suite* breakage (unrelated to contract logic) was found and
fixed. Residual-risk bound and uncovered surface are stated at the end.

---

## What was added

| File | Kind | Tests |
|---|---|---|
| `contracts/choice_clmm_pool/src/adversarial_fuzz.rs` | mock-backend model fuzzer | `fuzz_adversarial_invariants_many_seeds`, `fuzz_full_drain_cleanup` |
| `contracts/choice_clmm_pool/src/regime_tests.rs` | mock-backend regime tests | 13 tests (below) |
| `tests/manager_nft_attribution.rs` | test-tube (real bank + cross-contract) | `nft_fee_attribution_equal_liquidity_is_identical`, `..._proportional_unequal_liquidity` |

These build **on** the existing `solvency_fuzz.rs`, `manager_solvency_fuzz.rs`,
`malicious_cw20_blast_radius.rs` and `tick_math` property tests — they add the
*differential / model-based* layer (exact accounting, not just solvency) the
existing fuzzers lacked, plus the extreme regimes random streams never reach.

---

## The model-based fuzzer (`adversarial_fuzz.rs`)

Drives random interleavings of **every** fund-affecting entrypoint
(mint / swap-exact-in / low-level `Swap` with price limits / swap-exact-out /
burn / collect) against a native/native pool, with 4 simulated LPs. The harness
maintains an independent off-chain model of each LP's minted liquidity and, after
**every** op, reconstructs from that model every quantity the contract maintains
incrementally and demands an exact match. On failure it prints the seed + op
index and binary-search-**shrinks** to the minimal failing prefix.

Invariants asserted after every op:

| ID | Invariant | What it catches |
|---|---|---|
| INV-SOLV | pool never `Send`s more of a token than it holds (exact native ledger) | rounding-in-attacker-favor, double-withdraw, fee over-credit |
| INV-ACTL | `slot0.liquidity == Σ position.liquidity` over positions straddling the current tick | liquidity-accounting drift across mint/burn/cross |
| INV-NET | `Σ liquidity_delta` over all ticks `== 0` | non-telescoping deltas → mis-applied liquidity at a crossing |
| INV-GROSS | per tick: `active_positions_count == Σ` referencing-position liquidity | gross-liquidity / per-tick cap accounting |
| INV-DELTA | per tick: `liquidity_delta == Σ(+L lower) + Σ(−L upper)` | tick net-delta sign/magnitude errors |
| INV-BMAP | bitmap bit set **iff** a `TICKS` entry exists (both directions) | word-boundary bugs, stale bits, missed re-init |
| INV-DRAIN | after full burn+collect: zero active L, empty `TICKS`, empty bitmap, no residual owed | incomplete accounting cleanup / stranded state |

Scale: exercised at **1500 seeds × 450 steps (~675k ops)** clean. CI default is
200 × 400 (a few seconds); both are `const SEEDS` / `const STEPS` at the top of
the file.

## Regime tests (`regime_tests.rs`) — the extreme corners

1. **U256 fee-growth wraparound (the crown jewel)** —
   `fee_growth_wraparound_attribution_exact` and `..._with_crossing_exact`. Run
   the identical mint+swap twice: once with the global accumulator (and the
   in-range lower tick's `fee_growth_outside`) pre-loaded to `U256::MAX` so the
   swap's fee growth wraps past the modulus, once from zero. Asserts the credited
   fee is **byte-identical** across the wrap — including a variant that crosses a
   tick mid-wrap (exercising the `wrapping_sub` `fee_growth_outside` flip).
2. **Quote == execution** — `quote_matches_execution_exactly`,
   `quote_exact_output_matches_execution`. 400/300 random swaps: the read-only
   `Quote`/`QuoteExactOutput` must match the executed `amount_out`/`amount_in`
   exactly (fee pinned via `volatility_multiplier=0`).
3. **Constant-price round-trip never profits** —
   `constant_price_roundtrip_never_profits`. 2000 mint→burn cycles at unchanged
   price (biased to tiny liquidity); withdrawn ≤ deposited for both tokens.
4. **`MAX_LIQUIDITY_PER_TICK`** — `max_liquidity_per_tick_enforced`. Mint exactly
   the cap; one over reverts with the right error.
5. **Word-boundary crossing** — `word_boundary_cross_applies_liquidity`. A
   position whose lower edge sits exactly on a bitmap word boundary
   (`compressed ≡ 0 mod 256`, tick 2560 at spacing 10); walk price across it and
   assert active-L-from-tick exactly, proving the boundary tick's delta is
   applied (the historical bug skipped exactly word-aligned ticks).
6. **Extremes / full range** — `full_range_and_min_width_lifecycle`. Full-range
   and one-spacing-wide positions; swap both ways; full burn zeros active L.
7. **Protocol-fee carve conservation** —
   `protocol_fee_carve_conserves_and_never_shorts_lp`. With divisor 4:
   `protocol == floor(fee/4)`, accrued matches, LP gets the remainder (minus only
   `/2^128` flooring), `protocol + LP ≤ fee charged`.
8. **Flash** — `flash_lock_blocks_reentrancy` (lock blocks mint/burn/collect/
   flash/swap mid-loan), `flash_underpay_one_wei_reverts`,
   `flash_exact_repay_distributes_fee_and_clears_lock`.
9. **LP-vs-LP attribution (mock)** — `fee_attribution_proportional_between_lps`.
   Two LPs sharing a range, fees split exactly in proportion to liquidity;
   equal-L ⇒ byte-equal.

## Test-tube (`manager_nft_attribution.rs`) — regime 13

Per-NFT attribution exactness end-to-end through the real factory→pool→manager
reply chain and real bank: two NFTs collapsing onto one pool position, fees split
in **exact proportion to liquidity** (equal-L ⇒ identical), each NFT collects its
share, manager strands nothing. Complements `manager_solvency_fuzz` (which proved
the *aggregate* is solvent but not the *split*).

---

## Invariant / mutation table (non-vacuity proof)

Every new invariant was validated by breaking the relevant contract logic,
confirming the fuzzer/test **catches** it, then reverting. All mutations reverted;
contract sources are pristine (verified via `git diff`).

| Invariant / test | Mutation | Caught? | Reverted? |
|---|---|---|---|
| INV-ACTL | mint in-range add `>=`→`>` (boundary) | ✅ `INV-ACTL … active L != model (tick −2930)` | ✅ |
| INV-DELTA / INV-NET | mint upper-tick `checked_sub`→`checked_add` | ✅ `INV-DELTA tick 110 delta … != model` (3-op shrink) | ✅ |
| INV-GROSS | mint lower-tick `+amount`→`+amount+1` | ✅ `INV-GROSS tick −1190 gross … != model` | ✅ |
| INV-BMAP | burn skip `flip_tick` on last-position exit | ✅ `INV-BMAP bit … -> tick 3510 has no TICKS entry` | ✅ |
| INV-SOLV | collect drop `tokens_owed_0` decrement | ✅ `INV-SOLV pool Sends 1747365 uaaa but holds 3` | ✅ |
| fee-wrap (both) | `get_fee_growth_inside` `wrapping_sub`→`saturating_sub` | ✅ attribution diverged: control vs wrapped `0` | ✅ |
| constant-price no-profit | burn in-range `ceil+1` over-withdraw | ✅ `token0 extraction: deposited < withdrawn` | ✅ |
| LP-vs-LP attribution | `update_position` use fixed L instead of `position.liquidity` | ✅ `attribution not proportional` | ✅ |

(INV-NET and INV-DRAIN are also tripped transitively by the INV-DELTA and
INV-BMAP / INV-SOLV mutations respectively.)

Notable null result worth recording: mutating burn to round **up** (`false`→`true`)
does **not** trip the no-profit test, because mint already rounds up by the same
amount — `out == din` (break-even, no value created). Likewise `floor+1` yields
`out == ceil == din`. This is positive evidence that the mint-up / burn-down
asymmetry is calibrated to *exactly* the rounding needed for pool-favored dust and
no more.

---

## Findings

**No contract bug found.** One pre-existing issue, now fixed:

- **`tests/exploit_regressions.rs` — 5 tests broken by a prior address-hardening
  change (FIXED).** The pool's swap path now `addr_validate`s the output
  `recipient` (a hardening added in a previous run). Three swap helpers in this
  test file passed a literal `recipient: "trader".to_string()`, which cosmwasm
  2.2's bech32-strict `MockApi` rejects (`Error decoding bech32`), failing
  `crit4`, `crit5`, `ext_p1`, `ext_p2`, `ext_p4`. Fixed by using the in-scope
  bech32 `trader` Addr (`recipient: trader.to_string()`). Not a contract bug;
  real callers always pass valid bech32. All 10 now pass.

- **Note (not mine):** the working tree has a concurrent edit to
  `actions/swap.rs` adding an `ask_asset` output-attribute (`out_token.key()`) for
  the dex aggregator. It is additive (no accounting/math change) and does not
  collide with any attribute these tests parse; left as-is.

---

## Coverage & residual-risk bound

**Well-covered:** native/native pool accounting across all entrypoints; the V3
fee "outside model" including the full U256 wrap and tick crossings; tick bitmap
incl. word boundaries; per-tick gross/net liquidity and the per-tick cap;
mint-up/burn-down rounding (no value creation at constant price); quote↔execution
equivalence; protocol-fee carve conservation; flash lock/repay/fee; per-position
and per-NFT fee attribution exactness.

**Not exercised by the new fuzzers (residual risk):**
- *CW20 / mixed-asset pools* in the model fuzzer (native/native only). CW20 paths
  are covered by `malicious_cw20_blast_radius` + the manager test-tube suites; the
  CW20 `Receive`/`TransferFrom` *refund* branches in `apply_swap` are not in the
  differential model. **Low residual risk** (logic mirrors the native path).
- *Multiple tick spacings* in the random fuzzer (uses spacing 10). Spacings 1/60/
  200 and their distinct `MAX_LIQUIDITY_PER_TICK` are touched only by regime/unit
  tests. **Low.**
- *Extreme-tick density in random streams* (random fuzzer ranges within ±~9000
  ticks). `MIN_TICK`/`MAX_TICK`/full-range covered by regime tests, not the random
  sweep. **Low–moderate** for pathological many-thousand-tick crossings near the
  iteration cap (the cap itself has dedicated tests in `swap.rs`).
- *Dynamic-fee / EMA rate-limit dynamics* over multi-block time: the fee path is
  exercised every swap, but the `max_fee_change_per_second_ppm` clamp across
  varying `block.time` is covered only by the existing `oracle.rs` unit tests, not
  a time-advancing fuzzer. **Low** (pure, well-unit-tested arithmetic).
- *Factory* anti-squat / `Instantiate2` / fee-tier admin paths: out of the
  differential model (covered by `choice_clmm_factory/src/test.rs`).
- *Manager* interleaved multi-op stress is covered by the existing
  `manager_solvency_fuzz`; the new test adds attribution exactness but is not a
  randomized interleaving — mutation-validating manager logic requires a wasm
  rebuild (`./build_release.sh`) and was not performed (the underlying pool
  attribution it relies on **is** mutation-validated at the mock level).

Net: for the in-scope native pool accounting core, confidence is high. The
residual risk concentrates in CW20-asset refund branches, non-default tick
spacings, and pathological extreme-tick swap depth — all candidates for a future
sweep (parametrize the fuzzer over `(spacing, asset-types, tick-range)`).

---

## Reproduce

```bash
cd choice_exchange

# Model-based fuzzer + full-drain (fast CI default 200×400; crank the consts):
cargo test --release -p choice_clmm_pool adversarial_fuzz -- --nocapture

# All 13 regime tests (fee-wrap, quotes, dust, max-liq, word-boundary, flash, …):
cargo test --release -p choice_clmm_pool regime_tests

# Pre-existing exploit regressions (fixed) + all pool unit tests:
cargo test --release -p choice_clmm_pool

# Test-tube per-NFT attribution (needs docker-built artifacts/*.wasm):
cargo test -p choice-clmm-common --test manager_nft_attribution

# Math property/fuzz suite:
cargo test --release -p choice-clmm-math
```

To hunt harder: raise `SEEDS`/`STEPS` in `adversarial_fuzz.rs`. On any failure the
test prints `seed`, the minimal shrunk op prefix, and `FAILING ASSERTION: …`.

---
---

# Follow-up engagement — closing the residual-risk gaps

A second adversarial pass was commissioned to attack the corners the first pass
explicitly deferred (the "Coverage & residual-risk bound" list above):
CW20 / mixed-asset accounting, non-default tick spacings, extreme-tick swap
depth, the dynamic-fee rate limit over multiple blocks, the factory, and the
manager's own per-NFT bookkeeping. **Result: again no fund-safety or accounting
violation.** Every new invariant was mutation-validated (table below), including
the two that required a docker wasm rebuild (the manager, which the first pass
left unvalidated). All contract sources are pristine afterward (`git diff` over
`actions/`, `core/`, `math/`, the manager and factory `src/` is empty); the clean
`artifacts/choice_clmm_manager.wasm` is back to its canonical size.

## What was added

| File | Tier | Tests |
|---|---|---|
| `choice_clmm_pool/src/adversarial_fuzz.rs` | mock | `fuzz_adversarial_invariants_all_spacings` (item 2), `fuzz_extreme_tick_density` + `swap_iteration_limit_reverts_without_partial_state` (item 3); `fuzz_full_drain_cleanup` extended to all spacings |
| `choice_clmm_pool/src/regime_tests.rs` | mock | `max_liquidity_per_tick_enforced_per_spacing` (item 2); `dynamic_fee_rate_limit_fuzz`, `dynamic_fee_same_block_pin`, `dynamic_fee_manipulation_cannot_jerk_victim` (item 4); `cw20_receive_hook_partial_fill_refund_message_exact`, `cw20_allowance_swap_refunds_attached_native_message` (item 1, mock) |
| `tests/cw20_accounting_fuzz.rs` | test-tube | `cw20_accounting_fuzz_native_cw20`, `cw20_accounting_fuzz_cw20_cw20`, `receive_hook_partial_fill_refunds_unused_cw20`, `cw20_input_swap_refunds_wrongly_attached_native` (item 1) |
| `tests/factory_lifecycle.rs` | test-tube | `create_pool_address_matches_stored_and_config_is_canonical`, `duplicate_create_reverts`, `admin_entrypoints_are_owner_gated`, `antisquat_gate_blocks_then_allows_then_consumes` (item 5) |

## Per-item coverage

**Item 1 — CW20 / mixed-asset accounting (highest priority).** Two tiers:
- *Test-tube model fuzzer* over native/CW20 and CW20/CW20 pools (real `cw20-base`
  via `cw20_base_build.wasm`), settling from real bank + CW20 balances. Random
  mint (allowance) / low-level swap / **Receive-hook swap** / burn / collect; the
  pool's canonical token0/token1 order is read from the live config (never
  guessed). After every op the pool is solvent (`balance(tok) ≥ Σ tokens_owed`);
  after a full drain it owes nothing and strands ≤ a tight dust bound. Every
  Receive-hook swap asserts the **partial-fill refund is exact** (`Δ trader CW20
  == amount_in`). Focused tests pin the two refund branches: a liquidity-exhausted
  Receive-hook swap refunds the unused CW20; a CW20-input swap with wrongly
  attached native refunds the native in full (low-level `Swap` *and*
  `SwapExactOutput`, with the swap output routed to a third party so the sender's
  native delta isolates the refund).
- *Mock refund-message tests* drive `execute_swap_exact_input_cw20` /
  `execute_swap` directly and inspect the emitted `Cw20::Transfer` / `BankMsg::Send`
  refund messages — the SAME `apply_swap` branches, mutation-validated at cargo
  speed (no wasm), so the refund logic's non-vacuity is proven without a rebuild.

**Item 2 — non-default tick spacings (1/60/200).** The model fuzzer is
parametrized over `tick_spacing`; mint ticks are generated in compressed units
× spacing so every spacing straddles bitmap word boundaries. All 7 invariants
(SOLV/ACTL/NET/GROSS/DELTA/BMAP/DRAIN) re-run per spacing, and a per-spacing
`MAX_LIQUIDITY_PER_TICK` test mints exactly the (spacing-specific) cap and proves
one-over reverts.

**Item 3 — extreme-tick density & swap depth.** A biased generator piles
positions at `MIN_TICK`/`MAX_TICK`/full-range plus a dense band of adjacent
unit-width positions, then drives large swaps that cross hundreds of ticks; the
full invariant battery runs after every op (stressing the `u128↔i128`
`liquidity_net` sign at the extremes). A dedicated test seeds an initialized tick
at every spacing-1 tick past `MAX_SWAP_ITERATIONS`, fires an oversized swap, and
asserts it reverts with `SwapIterationLimit` **leaving the swap-relevant state
byte-identical** (no partially-applied crossing — structurally guaranteed because
`compute_swap` takes `&dyn Storage`, regression-guarded here).

**Item 4 — dynamic-fee / EMA rate-limit over multiple blocks.** A time-advancing
fuzzer mutates `block.time` between real swaps and reconstructs the oracle's
committed-fee/commit-time model, asserting after each swap: same-block swaps pay
the first swap's fee (anti-manipulation pin); the fee moves ≤ `RATE·Δt` from the
last committed fee; the fee stays in `[base, max]`. Focused tests pin the
same-block invariant under intra-block price moves and prove a single
price-slamming swap cannot jerk the next victim's fee beyond `RATE·Δt`.

**Item 5 — factory (real VM).** The factory's `src/test.rs` mock suite is
thorough; the test-tube suite adds what `mock_dependencies` cannot: the address
the pool is actually instantiated at (`Instantiate2`, salt = sha256(key0‖key1‖
fee)) equals the address the factory stores and serves from `GetPool`, and the
pool there carries the canonical token order + the fee tier's tick_spacing.
Plus end-to-end real reverts for duplicate creation, owner-gating of every admin
entrypoint (with a freshly-enabled fee tier then usable), and the anti-squat
`POOL_CREATION_AUTH` gate (reserved → blocks others → authorized create succeeds
and consumes the reservation → slot then rejects duplicates).

**Item 6 — manager per-NFT bookkeeping (mutation-validated via docker rebuild).**
The first pass could not mutation-validate the manager because that needs a
`./build_release.sh` rebuild; this pass did it. Three mutations were built into
`choice_clmm_manager.wasm` and run against `manager_nft_attribution` +
`manager_solvency_fuzz` (see the table). Net: the two suites are complementary —
attribution catches *non-proportional* per-NFT corruption, solvency catches
*over-crediting / over-collection* — and together they bite on both the per-NFT
liquidity factor and the `fee_growth_inside_last` snapshot.

## Invariant / mutation table (non-vacuity proof)

All mutations reverted; sources verified pristine via `git diff`. The three
manager mutations were applied to the wasm via `./build_release.sh` (docker), run,
then reverted with a clean rebuild (`manager.wasm` back to its canonical size).

| Item / test | Mutation | Caught? | Reverted? |
|---|---|---|---|
| 2 — all-spacings INV-BMAP | `flip_tick` compression spacing-blind (`tick/10`) | ✅ caught at **spacing 1 only** (spacing-10 sweep still passes): `tick 127 has a TICKS entry but bitmap bit (word 0,bit 127) is clear` | ✅ |
| 2 — per-spacing cap | cap formula ignores spacing (hardcode spacing 10) | ✅ `spacing 1: minting one over the cap must revert` | ✅ |
| 3 — extreme-density INV-ACTL | swap-crossing `add_liquidity` sign flip (`<0`→`>0`) | ✅ `INV-ACTL … active L 800001564093 != model 800004986085 (tick −16)` | ✅ |
| 3 — iteration-cap clean revert | persist `POOL_STATE` before `compute_swap` | ✅ `POOL_STATE mutated on iteration-limit revert … liquidity 1000 → 1001` | ✅ |
| 4 — rate limit | drop the per-second clamp (`clamped = raw_fee`) | ✅ `fee moved 579 ppm in 7s (> 350)`; victim fee jerked `3009 → 50000` (max) | ✅ |
| 4 — same-block pin | recompute fee mid-block instead of returning committed | ✅ `same-block swap #0 charged 3498 != committed 3000` | ✅ |
| 1 — Receive-hook refund (mock) | skip the `Cw20AlreadySent` partial-fill refund push | ✅ `partial fill must emit a CW20 refund Transfer to sender` | ✅ |
| 1 — attached-native refund (mock) | no-op `push_native_refund` | ✅ `wrongly-attached native not fully refunded: got 0 of 9999` | ✅ |
| 6 — manager (attribution + solvency) | reported-owed per-NFT liquidity → constant **(M2, docker)** | ✅ attribution `not proportional: 7/2050516626 vs 7/6151549880`; solvency `OVER-COLLECT … paid (57074,0) > owed (0,0)` | ✅ |
| 6 — manager solvency | skip `fee_growth_inside_last` snapshot → over-credit **(M3, docker)** | ✅ solvency `SHORTFALL token1 … owed 95285, paid 60749` (attribution unaffected — over-count is proportional) | ✅ |

*Recorded coverage edge (not a bug):* a fourth manager mutation — constant per-NFT
liquidity in the **execute-path** `accrue_fees_to_nft` (M1) — was **not** caught.
The attribution test reads owed via the read-only `PositionWithFees` query (which
uses the un-mutated `state.liquidity`) *before* any collect, and an execute-path
**under**-credit leaves the pool over-collateralized (still solvent) with the
shortfall retained in the pool rather than stranded in the manager. This is a
property of where these two suites assert, not a contract defect — the real
query and execute paths share the identical `L·Δfg/2^128` formula and both are
correct (M2 mutates the reported path and M3 the execute snapshot; both are
caught). Noted so the boundary is explicit.

*Test-authoring correction made during the engagement:* the first cut of
`cw20_input_swap_refunds_wrongly_attached_native` asserted the sender's net USDT
change was zero, but with a CW20 input the swap **output** is also USDT, so the
sender's balance legitimately rose. Fixed by routing the swap output to a separate
recipient, isolating the attached-native refund. (No contract change.)

## Findings

**No contract bug found** on any item. The CW20 refund branches, all four tick
spacings and their per-tick caps, extreme-tick deep crossings and the clean
iteration-cap revert, the dynamic-fee rate limit / same-block pin / manipulation
resistance, the factory's deterministic-address + owner-gating + anti-squat
behavior, and the manager's per-NFT liquidity and fee-growth-snapshot bookkeeping
all held — and each was proven non-vacuous by a reverted mutation.

## Coverage & residual-risk bound (updated)

The prior pass's residual list is now largely closed: CW20 refund branches
(mock-message + test-tube real-balance), non-default spacings (full battery),
extreme-tick depth + iteration cap, the dynamic-fee multi-block dynamics, the
factory (real-VM lifecycle), and the manager per-NFT bookkeeping (docker
mutation-validated) are all covered. Remaining, genuinely low residual:
- The reserved hook seam (`HOOK_BEFORE_SWAP` etc.) is inert (no engine, no
  setter) and out of scope.
- The manager execute-path under-accrual coverage edge noted above (not a bug;
  the formula is shared with the validated read path).
- CW20 `volatility_multiplier`/dynamic-fee interaction is exercised on native
  pools only; the fee math is asset-type-agnostic so this is purely defensive.

## Reproduce (follow-up)

```bash
cd choice_exchange

# Item 2/3 (mock, multi-spacing + extreme density + iteration-cap revert):
cargo test --release -p choice_clmm_pool adversarial_fuzz -- --nocapture
cargo test --release -p choice_clmm_pool regime_tests::regime_tests::max_liquidity_per_tick_enforced_per_spacing

# Item 4 (mock, time-advancing dynamic-fee fuzzer + focused):
cargo test --release -p choice_clmm_pool dynamic_fee

# Item 1 (mock CW20 refund-message tests):
cargo test --release -p choice_clmm_pool cw20_

# Item 1 (test-tube CW20 accounting — needs docker-built artifacts/*.wasm):
cargo test -p choice-clmm-common --test cw20_accounting_fuzz

# Item 5 (test-tube factory lifecycle):
cargo test -p choice-clmm-common --test factory_lifecycle

# Item 6 (manager attribution + solvency; mutation-validate by rebuilding the
# manager wasm with ./build_release.sh after editing accrue_fees_to_nft):
cargo test -p choice-clmm-common --test manager_nft_attribution --test manager_solvency_fuzz
```
