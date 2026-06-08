# Native test tooling — coverage, mutation, fuzzing

Three tools strengthen the CLMM math/state tests. **All three run on the NATIVE
host target (`cargo test`), never through wasm / `injective-test-tube`.**

> ⛔ **Never invoke the wasm optimizer in any of these loops.** Source mutations
> are invisible to compiled bytecode, and rebuilding wasm per mutant/fuzz-input
> is prohibitively slow. Do **not** run `./build_release.sh`, the
> `cosmwasm/optimizer` Docker image, or `wasm-opt` while coverage/mutation/fuzz
> is running. `make build-all` (the per-contract wasm build) is also unrelated
> and unnecessary here.

## One-time prerequisite — the `ed25519-zebra` host-build fix

`ed25519-zebra 4.1.0` (pulled in by `malicious_cw20`) no longer enables its
`alloc` feature via `std`, but `cosmwasm-crypto 2.2.2` needs the `alloc`-gated
`batch` module. Without a fix, **any host-target `cargo test` fails to compile**
(the wasm build is unaffected, which is why this hides until you test natively).

Fix is already committed as a **dev-dependency** (so the wasm cdylib is
untouched) in `packages/choice_clmm_math/Cargo.toml` and
`contracts/choice_clmm_pool/Cargo.toml`:

```toml
[dev-dependencies]
ed25519-zebra = { version = "4.1.0", features = ["alloc"] }
```

The `fuzz/` crate carries the same line. Do not remove these.

## What is "native" vs "test-tube" here

| Crate | Native tests (host) | Pulls test-tube? |
|---|---|---|
| `choice-clmm-math` | `src/**` unit tests + `tests/v3_*_vectors.rs` | no |
| `choice_clmm_pool` | `--lib`: `tests.rs`, `solvency_fuzz`, `adversarial_fuzz`, `regime_tests` | no |
| `choice_clmm_common` | `tests/*.rs` integration | **yes — excluded from all 3 tools** |

Always scope the tools with `--package` / `-p` so they never fall back to a
whole-workspace build that would compile the test-tube integration suite.

---

## 1. Coverage — `cargo-llvm-cov`

Already installed (`cargo install cargo-llvm-cov`; needs the `llvm-tools`
rustup component).

```bash
cd choice_exchange

# Math + pool core, native only, with branch coverage. The pool's --lib tests
# also exercise the math crate (e.g. bitmap walk hits least_significant_bit),
# so run them together for the truest math coverage picture.
cargo llvm-cov --branch \
  -p choice-clmm-math \
  -p choice_clmm_pool --lib \
  --summary-only

# Math crate alone (fast, < 20s):
cargo llvm-cov --branch -p choice-clmm-math --summary-only

# Re-report uncovered lines from the last run WITHOUT re-running tests:
cargo llvm-cov report --show-missing-lines \
  --ignore-filename-regex 'tests?\.rs|_fuzz\.rs|regime_tests\.rs'

# HTML report:
cargo llvm-cov --branch -p choice-clmm-math -p choice_clmm_pool --lib --html
# -> target/llvm-cov/html/index.html
```

---

## 2. Mutation testing — `cargo-mutants`

Installed via `cargo install cargo-mutants`. Shared knobs live in
[`../mutants.toml`](../mutants.toml). **Always pass `--package`.**

> ⚠️ **Disk note.** By default cargo-mutants copies the whole source tree per
> parallel job. This tree is **not a git repo**, so it cannot filter via
> `.gitignore` and will copy the ~36 GB `target/` (and nested `target/` dirs like
> `chain_capability_harness/target`) once per `-j` worker — which fills the disk.
> Two safe options:
>   1. **`--in-place`** (recommended here): mutate the original tree, no copy,
>      no disk blowup. Implies serial (cannot combine with `-j`). Used below.
>   2. Or `git init` the tree (so copies skip `target/`) **and/or**
>      `cargo clean` the nested `chain_capability_harness/target` first, then use
>      `-j`.

```bash
cd choice_exchange

# (a) Pure math — fast suite (lib + v3 vectors), ~440 mutants.
cargo mutants --package choice-clmm-math \
  -f 'packages/choice_clmm_math/src/**/*.rs' \
  -j 6 --timeout-multiplier 5 --output mutants.out.math

# (b) Pool core logic — EXCLUDE the slow native fuzzers (otherwise each mutant
#     runs the 5+ min adversarial/solvency fuzz and the job never finishes).
cargo mutants --package choice_clmm_pool \
  -f 'contracts/choice_clmm_pool/src/core/**/*.rs' \
  -f 'contracts/choice_clmm_pool/src/actions/swap.rs' \
  -j 6 --timeout-multiplier 5 --output mutants.out.pool \
  -- --lib -- --skip adversarial_fuzz --skip solvency_fuzz

# List mutants without running them:
cargo mutants --list --package choice-clmm-math -f 'packages/choice_clmm_math/src/**/*.rs'
```

Results: `mutants.out*/` → `caught.txt`, `missed.txt`, `timeout.txt`,
`unviable.txt`, `outcomes.json`. **Surviving (missed) mutants** are the
test-quality gaps to fix with new assertions.

---

## 3. Fuzzing — `cargo-fuzz` (nightly)

Installed via `cargo install cargo-fuzz`; requires the nightly toolchain
(already the workspace default). Targets fuzz the **pure math only** — never a
contract entry point or wasm. They encode real invariants (see each target's
header), not just "doesn't panic".

```bash
cd choice_exchange/packages/choice_clmm_math

cargo +nightly fuzz list          # full_math bit_math tick_math sqrt_price_math swap_step liquidity_math
cargo +nightly fuzz build         # build all targets (host + libFuzzer/ASan)

# Short smoke campaign (CI-friendly). DO NOT run unbounded campaigns unattended.
cargo +nightly fuzz run liquidity_math -- -max_total_time=60

# Reproduce a saved crash:
cargo +nightly fuzz run liquidity_math fuzz/artifacts/liquidity_math/crash-<hash>
```

| Target | Invariants asserted |
|---|---|
| `full_math` | floor/ceil never over-credit; `ceil-floor ≤ 1`; exactness |
| `bit_math` | MSB/LSB are the true high/low set bits; zero ⇒ `Err` |
| `tick_math` | tick↔sqrt monotonic + round-trips; price brackets `[ratio(t), ratio(t+1))` |
| `sqrt_price_math` | price monotonic & non-zero; implied input ≤ supplied; "owe" leg rounds up |
| `swap_step` | `amount_in+fee ≤ remaining`; never overshoot price limit; exact-out caps delivery |
| `liquidity_math` | **solvency:** mint-then-burn (round-down) never returns more than deposited |

Crash corpus is saved under `fuzz/artifacts/<target>/`; seed corpus under
`fuzz/corpus/<target>/`.
