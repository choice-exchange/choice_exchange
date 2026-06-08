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
