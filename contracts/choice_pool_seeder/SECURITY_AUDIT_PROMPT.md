# Security Audit Prompt — `choice_pool_seeder` (CLMM graduation + locker)

Paste this into a fresh session at repo root `/home/dan/workspace/injective`.

---

You are doing a **security audit** of the `choice_pool_seeder` CosmWasm contract on Injective. It was just extended to graduate launches to a **CLMM (concentrated-liquidity) pool** and to hold **locked liquidity that still collects trading fees**. The new surface handles real value — a sink transiently holds the launch token + pair asset, and a locker permanently owns a CLMM position NFT and can collect fees from it — so correctness and abuse-resistance matter. Find the bugs before mainnet.

## Scope (audit these)

Primary, in `choice/choice_exchange/contracts/choice_pool_seeder/src/`:
- `clmm.rs` — `init_sqrt_price_from_amounts` (Q64.96 sqrt price via `Uint512::isqrt`) and `full_range_ticks`. **Math-critical.**
- `contract.rs` — `settle_clmm`, `settle_xyk`, `split_tip`, `callback_sweep_dust`, `exec_create_sink`, `exec_create_locker`, `exec_collect_fees`, `exec_update_beneficiary`, `require_pool_kind_matches_factory`, `query_fee_tier_spacing`, `query_clmm_pool`, the role-dispatch helpers, and the 3-variant `instantiate`.
- `msg.rs` / `state.rs` / `error.rs` — `PoolKind`, `LockerInit`, `FactoryConfig` CLMM pins, `PoolKindStored`, `LockerConfig`, role enum.
- `tests.rs` (assess coverage adequacy, not correctness of the contract).

Context (read to verify cross-contract assumptions, but they're separately audited — don't re-audit their internals):
- `packages/choice_clmm_common/src/{factory,manager,types}.rs` — the `CreatePool` / `MintPosition` / `Collect` / `GetPool` / `GetFeeTiers` / `Tokens` message shapes and the `AssetInfo` ordering.
- `contracts/choice_clmm_factory/src/contract.rs` (`execute_create_pool`, `GetPool`) and `contracts/choice_clmm_manager/src/contract.rs` (`execute_mint_position`, `execute_collect`, native-funds refund path, owner/approval checks). **Confirm the seeder's assumptions about these actually hold.**
- `docs/clmm_graduation_plan.md` — design intent + as-built notes.

Out of scope: the CLMM pool/manager/factory internals, the issuer, the EVM launchpad.

## Architecture & trust model (so you can reason about abuse)

Single binary, three roles fixed at instantiate (`Role::Factory|Sink|Locker`), dispatched via `require_factory/require_sink/require_locker`.

- **Factory**: admin-controlled; pins one XYK `choice_factory` and optionally a CLMM `(clmm_factory, clmm_manager)` pair. Spawns sinks/lockers via `Instantiate2` (no contract admin → immutable). `CreateSink`/`CreateLocker` are **permissionless** — security rests on deterministic salt addressing + validation that the payload's DEX addresses match the factory's pins.
- **Sink**: one-shot. Holds the launch token (`token_denom`) + pair asset (`pair_denom`). `Settle` is **permissionless** (a tip incentivises crankers). CLMM `Settle`: compute initial price from the seed ratio → `CreatePool` → `MintPosition` (full range, NFT to `position_recipient`) → `SweepDust`. `Refund` permissionless after a deadline.
- **Locker**: permanently owns the position NFT. `CollectFees` **permissionless**, routes fees to a single `beneficiary` via `manager.Collect { recipient }`. No decrease/burn/transfer path → principal locked. `UpdateBeneficiary` admin-gated (admin optional).

Funds the contract touches: bank balances of two native denoms in the sink; the position NFT in the locker; fees flow pool→manager→beneficiary and never rest in the locker.

## What to hunt for (non-exhaustive — think adversarially beyond this list)

**Math (`clmm.rs`) — highest priority:**
1. `init_sqrt_price_from_amounts`: overflow/underflow on `amount1 << 192` and the `Uint512`→`Uint256` narrowing; precision loss from `isqrt`; correctness of the clamp to `[MIN_SQRT_RATIO, max_sqrt-1]`. Does the produced price ever land **outside** what the pool's `get_tick_at_sqrt_ratio` accepts (→ `CreatePool` reverts)? Prove the bounds with concrete extreme inputs (1 vs `u128::MAX`, and realistic 18-dec amounts).
2. When the price **is** clamped, the seed ratio no longer matches the pool price → lopsided mint / large dust. Is that exploitable (e.g., attacker forces a degenerate ratio so most value is swept to `refund_receiver` or stranded)? Who controls the seed amounts, and can they be skewed before `Settle`?
3. `full_range_ticks`: integer division toward zero at negative `MIN_TICK` — confirm both bounds stay in-range and are exact multiples of every real tick spacing (1/10/60/200) **and** any custom spacing the factory owner could enable.

**Settle flow (`settle_clmm`):**
4. **Token sort parity**: the seeder sorts `(token_denom, pair_denom)` with `ClmmAssetInfo`'s `Ord`. Confirm it matches the CLMM factory's ordering exactly — a mismatch means `token0/token1` and the funds/amount mapping disagree (wrong price orientation, wrong amounts).
5. **`amount*_min = 0`** in `MintPosition` (no slippage floor). The justification is "pool created at our price in the same atomic tx." Stress this: can anything execute *between* `CreatePool` and `MintPosition` (sub-message ordering, a hook, the manager re-resolving a *different* pool) that moves the price or routes funds to an attacker?
6. **Pre-existing-pool guard** (`query_clmm_pool` → `ClmmPoolAlreadyExists`): `GetPool` errors are treated as "absent". Does that mask real failures (factory unreachable, malformed) and let `Settle` proceed into a bad state? Is the guard itself front-runnable for griefing (attacker pre-creates the pool to block graduation), and is that an acceptable DoS given `Refund`? Is there a TOCTOU between the guard query and the in-tx `CreatePool`?
7. **Funds accounting**: CLMM `Settle` rejects any `info.funds`; XYK still requires the exact create-fee. Confirm the balances read are the seed (not inflated by attached funds), the tip is taken only from the pair side, and the `MintPosition` funds (`token0/token1` amounts) exactly equal the contract's available balance after the tip send, in message order.
8. **`SweepDust`**: can it misroute or strand value? It sends leftover to `refund_receiver` — is `refund_receiver` trusted for the CLMM path the same way it is for XYK? Re-entrancy / status already terminal?
9. **State machine**: `Settle` flips `Settled` before dispatch — confirm no path leaves a sink half-settled with value but unable to retry or refund. Interaction of `Settle` vs `Refund` terminal states.

**Locker:**
10. `CollectFees { token_id: None }` enumerates `manager.Tokens` capped at 30 — silent truncation if a locker ever holds >30 positions (some fees uncollectable via `None`, though `Some` works). Real risk for a launchpad locker (holds one)? Worth a log/err?
11. Can a locker be pointed at a hostile `manager`? (`CreateLocker` pins `manager` to the factory's `clmm_manager` — but a *directly* instantiated locker doesn't go through `CreateLocker`. Does that matter given addressing?)
12. `UpdateBeneficiary` auth (admin optional → immutable). Permissionless `CollectFees` — any griefing angle (fees always go to `beneficiary`, so spamming just wastes the caller's gas — confirm)?
13. Locker never asserts it owns the `token_id` it's told to collect — relies on the manager's owner check. Confirm the manager rejects non-owner collects so this can't be abused.

**Role & cross-cutting:**
14. 3-variant role dispatch — any handler reachable from the wrong role, or a query/exec that loads the wrong `Item`? `Callback` self-gating intact?
15. Did the `SinkInit` restructure (moving `choice_factory`/`lp_destination` into `PoolKind::Xyk`) silently weaken any previously-audited XYK invariant? Diff the XYK path behavior against the prior version (`git`/the prior tests) — same messages, same guards, same fee handling.
16. `require_pool_kind_matches_factory` and `ClmmHalfConfigured` — can a factory be configured (or a sink created) such that a CLMM sink targets unpinned/attacker addresses?
17. Anything that assumes native denoms but could be fed a CW20 / weird denom; `multiply_ratio` rounding on the tip; `checked_*` coverage on every arithmetic op.

## Method

1. Read the scoped files in full; read the CLMM factory/manager entry points to **verify** (don't assume) the manager refunds native surplus to `info.sender`, mints the NFT to `recipient`, and gates `Collect` on ownership; and that `CreatePool` takes no funds and reverts on duplicates.
2. For the math, derive bounds analytically and corroborate with throwaway tests (`cargo test -p choice-pool-seeder --lib`). Add adversarial test cases where useful.
3. For each finding give: **severity** (Critical/High/Medium/Low/Info), **file:line**, a concrete **exploit or failure scenario**, and a **concrete fix**. Adversarially try to *refute* your own High/Critical findings before reporting them — note residual uncertainty.
4. End with an explicit **sign-off on the value-handling invariants**: (a) a sink can never send seed funds anywhere except the pool/tip/refund_receiver/issuer per its config; (b) a locker can never move principal, only route fees to its beneficiary; (c) the seeded pool price always equals the seed ratio (or is safely clamped) and a full-range mint can't be made lopsided by an attacker; (d) no role/state path strands value irrecoverably.

Build/test: `cd choice/choice_exchange && cargo test -p choice-pool-seeder --lib`. (Integration tests need `make build-all` artifacts and the CLMM-stack wasm; `cargo test --test integration -p choice-pool-seeder` then drives the full XYK and CLMM cross-contract lifecycles against real factory/pair/pool/manager deployments.)

Produce a single ranked findings report. Do not change contract logic except throwaway tests to prove a finding — propose fixes, don't apply them, unless asked.
