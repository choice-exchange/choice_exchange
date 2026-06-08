# CLMM native test-tooling findings

Generated from `cargo-llvm-cov`, `cargo-mutants`, and `cargo-fuzz` run on the
**native host target only** (no wasm / test-tube). Reproduce with the commands in
[native_test_tooling.md](native_test_tooling.md).

> **No exploitable library bug found.** One fuzz crash was a **harness
> false-positive** (Phase 4). One latent correctness gap (**F-1**, below) was
> surfaced — a dead/incorrect overflow fallback that was **not reachable with the
> contract's `Uint128`-bounded amounts** (so never exploitable). It has since been
> **FIXED** (authorized follow-up). Everything else is **coverage / test-quality
> gaps**, ranked by fund-safety severity for a CLMM.

## F-1 (latent, non-exploitable) — token1 overflow fallback was dead code — FIXED

`sqrt_price_math::get_next_sqrt_price_from_amount1_rounding_down` guards its
`amount << 96` overflow with `amount.checked_shl(96)` and falls back to `mul_div`
in the `else` arm. But `Uint256::checked_shl(96)` errors **only when the shift
amount (the constant 96) is ≥ 256** — it never detects a *value* overflow; it
silently wraps (`(Uint256::ONE << 200).checked_shl(96) == Ok(0)`, verified). So:

* the `mul_div` fallback (sqrt_price_math.rs lines ~185 and ~196) is **unreachable**;
* if `amount ≥ 2^160` ever reached this function, it would compute a **wrong
  (wrapped) quotient** instead of falling back.

**Why it was not exploitable:** the pool's token amounts are `Uint128`-bounded
(`amount << 96 ≤ 2^224 < 2^256`), so the overflow never occurs. The sibling
token0 path uses `checked_mul` (which *does* detect value overflow) and its
fallback is correct and reachable (covered by a test).

**Fix applied** (authorized): both `add` and `sub` branches now guard with
`amount.checked_mul(Q96)` instead of `amount.checked_shl(RESOLUTION)`. Since
`amount << 96 == amount * Q96`, this is identical for all valid inputs but
*detects* the value overflow, so the `mul_div` fallback is now reachable and
never wraps — full parity with the token0 path. Two regression tests
(`amount1_add_overflow_fallback_is_correct`,
`amount1_sub_overflow_fallback_is_correct`) drive `amount = 2^200` through the
fallback and assert hand-computed results. Clippy clean; all fuzz targets
re-smoked clean post-fix.

## Severity-ranked gaps

| # | Severity | Area | Finding | Suggested fix |
|---|---|---|---|---|
| 1 | **HIGH** | `liquidity_math.rs` | **0% native coverage** — every function (`get_liquidity_for_amount0/1`, `get_liquidity_for_amounts`) is untested on the host by *any* native test (math lib, v3 vectors, or pool `--lib`). It is exercised only through test-tube. This is the mint-sizing kernel: a rounding error over-credits an LP's liquidity → drainable insolvency. Mutation confirms it: its mutants survive wholesale. | Add direct unit tests (V3 reference vectors) + the new `liquidity_math` fuzz target (already encodes the mint-then-burn solvency invariant). |
| 2 | **MED** | `bit_math::least_significant_bit` | **~64 surviving mutants** under the math crate's own tests — you can flip `&`→`\|`, `<<`→`>>`, `-=`→`+=` across the whole function and no `cargo test -p choice-clmm-math` test fails. It has no direct unit test; it's reached only via the pool's bitmap walk (so it shows ~98% *line* coverage in the combined run, but line-execution ≠ value-assertion). A wrong LSB skips an initialized tick → liquidity silently not crossed (same class as the prior word-boundary bug). | Add a direct LSB unit test with known bit patterns + the new `bit_math` fuzz target (asserts the exact lowest set bit). |
| 3 | **MED** | `sqrt_price_math.rs` | **74% branch.** Uncovered: the **overflow-safe fallback paths** in `get_next_sqrt_price_from_amount0_rounding_up` (lines 132–138) and `..amount1..` (185, 196) — reached only with very large amounts/prices — plus the zero-price / zero-liquidity / "price can't go up/down" error guards (107–114, 146–148, 171–173, 246–253). The fallbacks are protocol-critical (they keep the price math exact at the extremes). | New `sqrt_price_math` + `swap_step` fuzz targets push large inputs into the fallbacks; add explicit unit tests for each error guard. |
| 4 | **MED** | `swap_math.rs` | **78% branch.** Uncovered: the `one_for_zero` direction of several step branches (lines 75/82/110/117/128/135), the exact-out paths (190–273), the defensive "rounding invariant violated" / "fee underflow" guards (156–158), and the exact-out direction-mismatch error (202–204). | New `swap_step` fuzz target covers both directions + exact-out; add unit tests for the two defensive guards. |
| 5 | LOW | `tick_math.rs` | 92% branch — solid (v3 vectors + existing fuzz). Residual uncovered helpers: sign-magnitude arith edges (lines 248, 269, 289) and the `#[ignore]`d exhaustive roundtrip (540–546). | Optional: targeted unit tests for the sign-magnitude helpers. |
| 6 | LOW | `full_math.rs` | 100% branch / 94% region. Minor: the exact-division remainder-zero arm in `div`/`mul_div_round_up` (lines 24–25, 51, 54). | Covered going forward by the `full_math` fuzz target. |

## Phase 2 — coverage (native, branch), key files

Measured with the full native suite (`-p choice-clmm-math -p choice_clmm_pool --lib`):

| File | Region | Line | Branch |
|---|---|---|---|
| `liquidity_math.rs` | **0.00%** | **0.00%** | — |
| `sqrt_price_math.rs` | 88.2% | 84.8% | **74.1%** |
| `swap_math.rs` | 91.4% | 86.4% | **77.8%** |
| `tick_math.rs` | 89.4% | 93.6% | 92.1% |
| `bit_math.rs` | 97.9%¹ | 97.8%¹ | 94.4%¹ |
| `full_math.rs` | 93.9% | 88.6% | 100% |

¹ Lifted by the pool's `--lib` tests; **math-crate-only** it is ~40% (relevant
because mutation testing is per-package — see gap #2).

Pool core modules are well covered by native `--lib` tests: `core/bitmap.rs`
95.9%, `core/ticks.rs` 96.2%, `core/positions.rs` 85.7%, `core/oracle.rs` 89.0%,
`actions/swap.rs` 89.0%. `contract.rs` (58%) is entry-point dispatch reached
mostly via test-tube — out of scope for native math testing.

## Phase 3 — mutation (math crate, `cargo test -p choice-clmm-math`)

**Baseline (before new tests):** 444 mutants → **243 caught, 109 missed, 92
unviable** (0 timeout). Catch rate on viable mutants = 243/352 = **69%**.

Surviving (missed) mutants by file — survivors concentrate exactly in the
coverage gaps above:

| File | Missed | Top function(s) |
|---|---|---|
| `bit_math.rs` | 64 | `least_significant_bit` (whole function — no math-crate test) |
| `liquidity_math.rs` | 18 | `get_liquidity_for_amount0/1`, `get_liquidity_for_amounts` (0% covered) |
| `tick_math.rs` | 12 | sign-magnitude helpers (`signed_sub_abs`, `signed_or_bit`, `u512_low_256`) |
| `sqrt_price_math.rs` | 8 | `get_next_sqrt_price_from_amount0_rounding_up`, `get_amount0_delta` |
| `swap_math.rs` | 6 | `compute_swap_step_exact_out` |
| `utils.rs` | 1 | — |

**After adding the recommended tests** (this work), re-ran `--in-place` on the two
top-survivor files (`bit_math.rs` + `liquidity_math.rs`):

* **Before:** 82 missed (64 `least_significant_bit` + 18 `liquidity_math`).
* **After:** **0 real survivors.** 115 caught, 26 unviable, and **4 equivalent
  mutants** (provably no behavioral change, so unkillable by any test):
  * `bit_math.rs:97 >>= → <<=` — the *final* `n >>= 1` in `least_significant_bit`,
    after `r` is already decided; `n` is never read again, so the shift is dead.
  * `liquidity_math.rs:15/47/82 < → <=` — the price-ordering swap
    `if a < b {..} else {..}`; the only differing input is `a == b`, which routes
    to the same `upper == lower` error either way.

So mutation score on these files went from 64/146 ≈ 44% to **115/115 = 100%** of
non-equivalent mutants killed.

## Tests added (this work)

New `#[cfg(test)]` units, all green (53 lib + 42 vector tests pass), targeting the
survivors/branch-gaps — no library source changed:

* `bit_math.rs` — exhaustive single-bit MSB/LSB over all 256 positions + mixed-bit
  + zero-error + `Uint256::MAX` cases. Kills the 64 `least_significant_bit` (and
  MSB) survivors.
* `liquidity_math.rs` — closed-form reference values at power-of-two prices for all
  three functions + all three price regimes + equal-bound error + the **mint→burn
  solvency** invariant with inexact rounding. Kills the 18 survivors.
* `sqrt_price_math.rs` — error guards (zero price, zero liquidity, price-can't-rise)
  + the **reachable** amount0 overflow fallback (`checked_mul`).
* `swap_math.rs` — exact-in zero-price guard, `zero_for_one` full/partial steps,
  exact-out direction guard + `zero_for_one` full/partial.

(Pool-core mutation run is the documented fuzz-excluded command in
[native_test_tooling.md](native_test_tooling.md); on this disk it must use
`--in-place`.)

### After-coverage (math crate alone, with new tests)

Mutation is per-package, so the math crate's *own* suite is what matters. Branch
coverage moved **78% → 90%** overall:

| File | Branch before | Branch after | Line after |
|---|---|---|---|
| `bit_math.rs` | 47% | **100%** | 100% |
| `liquidity_math.rs` | 0% (line) | **93%** | **98%** |
| `sqrt_price_math.rs` | 76% | **87%** | 93% |
| `swap_math.rs` | 78% | **81%** | 92% |

(`tick_math` 91% and `utils` unchanged — not targeted this round.)

## Phase 3b — pool-core mutation (fuzz-excluded)

`actions/swap.rs` + `core/ticks.rs` + `core/positions.rs`, `--in-place`, with
`adversarial_fuzz`/`solvency_fuzz` **skipped for speed**: 173 mutants → 99 caught,
7 timeout (loop-condition mutants in `compute_swap` — effectively caught), **50
missed**, 17 unviable.

> **Caveat — these 50 are overstated.** Skipping the two fuzzers removes the
> pool's strongest invariant checks (solvency + accounting), which exercise
> exactly these paths. The fund-critical fee-accounting core barely moved:
> `core/ticks::get_fee_growth_inside` (2) and `core/positions::update_position`
> (2). The other 46 are in `actions/swap.rs` handler/query plumbing
> (`execute_swap_*`, `query_quote_*`) — response attributes, recipient routing,
> quote formatting — much of which the skipped fuzzers cover via balance checks.
> **A definitive pool-core mutation score needs the fuzzers included** (run on a
> CI box; baseline suite ≈ 316 s/mutant). Treat this run as a fast, conservative
> lower bound, not the final word.

## Phase 4 — fuzzing (6 targets, ~60s smoke each)

All targets assert real invariants (not just "no panic"). Results:

| Target | Runs (60s) | Result |
|---|---|---|
| `full_math` | 4.27M | clean |
| `bit_math` | 13.7M | clean |
| `tick_math` | 0.41M | clean **after harness fix** (see below) |
| `sqrt_price_math` | 1.39M | clean |
| `swap_step` | 1.19M | clean |
| `liquidity_math` | 1.89M | clean |

**The one crash — a harness false-positive, not a bug.** `tick_math` initially
crashed on the all-`0xff` input (→ `tick == MAX_TICK`). The harness asserted the
round-trip `get_tick_at_sqrt_ratio(get_sqrt_ratio_at_tick(MAX_TICK)) == MAX_TICK`.
But `get_sqrt_ratio_at_tick(MAX_TICK) == max_sqrt_ratio()`, and
`get_tick_at_sqrt_ratio` treats the max as **exclusive** by design (Uniswap V3's
half-open domain: MIN inclusive, MAX exclusive — the library has explicit tests
for this). The library is correct; the harness over-asserted. Fixed to encode the
asymmetry (at `MAX_TICK` the inverse *must* reject), then 0.41M runs clean.
