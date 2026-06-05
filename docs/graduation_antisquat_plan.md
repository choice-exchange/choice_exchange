# Graduation anti-squat: namespace-auth gate + entropy

**Status:** design proposal (not built). Scope: protects the EVM→Choice graduation
on-ramp (`choice_mts_issuer` + `choice_pool_seeder` + `choice_clmm_factory`) against
pool / sink / locker front-running.

**Companion docs:** `clmm_graduation_plan.md` (the graduation flow as built),
`choice_pool_seeder/SECURITY_AUDIT_PROMPT.md` (where M-1/M-2 were found).

---

## 1. Problem

The on-ramp lets any EVM project graduate into a Choice CLMM pool. The
denom and the sink are **born at the start of the launch, not at graduation** —
this timing is the crux of the whole problem, so spell it out:

1. EVM `createLaunch` → `LaunchReserved` (sequential `launchId`).
2. The keeper, at the `Observed` stage, immediately calls
   `choice_mts_issuer.RegisterLaunch`. This single tx mints the tokenfactory
   launch denom (`MsgCreateDenom`), MTS-links it to an auto-deployed ERC20
   (`MsgCreateTokenPair`), and spawns the `choice_pool_seeder` **sink** (via
   `Instantiate2`). **From here the denom and sink address are public.**
3. `bindLaunchToken` → the bonding curve trades (potentially hours/days).
4. `CurveFilled` → `triggerGraduation` → `BootstrapReady`.
5. The keeper `Settle`s the sink — **only now** is the CLMM pool created (at the
   seed ratio) and a full-range position minted to a **locker**.

So the denom and sink exist, public, for the **entire curve lifetime** (steps
3–4) before the pool is created at step 5. This is a **reusable product**, so any
weakness here is inherited by *every* future graduation, not just the first
consumer (SHROOM).

Two front-running vectors, both rooted in **predictability** — but note they open
at *different* points in the timeline above:

### M-1 — CLMM pool squat (window: the whole curve lifetime, steps 2→5)
`choice_clmm_factory::execute_create_pool` is permissionless, takes no funds
(free), and does **not** require the native denom to exist — verified: no
`info.sender` check, `funds: vec![]`, and pool keys are plain strings never
checked against the bank. The launch denom is
`factory/{issuer}/{prefix}_{internal_id}` with a sequential `internal_id`, and
the `fee_tier` is snapshotted into the sink at creation, so the full
`(launch_denom, pair, fee)` key is **public the moment `RegisterLaunch` lands
(step 2)**. An attacker reads it from the event and creates the pool at a junk
price any time during the **entire curve lifetime** before `Settle` (step 5).
The seeder then hits `ClmmPoolAlreadyExists` → graduation is blocked → only
`Refund` remains. (CLMM pools cannot be re-priced after creation, and seeding
into a pre-priced pool would be lopsided, so the guard is correct — the problem
is the resulting DoS.) **Note the window opens at step 2 but the pool isn't
created until step 5; this gap is what makes Layer A insufficient for M-1 — see
§2.**

### M-2 — sink / locker address squat (window: front-run of `RegisterLaunch`, step 2)
`WasmMsg::Instantiate2` derives the contract address from
`(creator, code_id, salt)` and **not** the init msg (`fix_msg = false`). The sink
salt convention is `encode(issuer_addr, internal_id)` — predictable. Confirmed
exploitable: `choice_pool_seeder::exec_create_sink` ignores `info.sender` (it
only checks the contract's own factory role) and passes the **caller-supplied
`salt`** straight into `Instantiate2`, so an outsider can call the factory's
permissionless `CreateSink` to pre-instantiate the sink (or locker) at its
canonical address with junk config; `RegisterLaunch`'s `CreateSink` then collides
and the whole launch tx reverts. Griefing/DoS (not theft — the issuer both
creates and funds the sink atomically, so a squatted address never gets funded).
Because the sink is created **inside** `RegisterLaunch` (step 2), entropy in the
salt genuinely reduces this to a same-block front-run of that tx — unlike M-1.

**Severity:** both are griefing/DoS, recoverable via `Refund` / retry, but
trivially cheap to mount against a *predictable, sequential* target — and they
hit the entire product line. Their mitigations differ: M-2 is closed by entropy
(Layer A); M-1 is closed *only* by the factory gate (Layer B), because M-1's
attack window opens after the denom is already public (see §2).

### Why not solved elsewhere
- **EVM-V4 graduation** (V4 hooks `beforeInitialize` would namespace the pool):
  rejected — that AMM isn't ours, and graduating there defeats the product's
  purpose (capture EVM launches into *Choice* liquidity).
- **Issuer/seeder alone** cannot fully close M-1: the factory is permissionless
  and the EVM→CW pair-asset hop ("Leg C") forces a multi-tx flow, so the denom
  is public (post-`RegisterLaunch`) before the pool is created (at `Settle`).
  Closing that residual needs a factory-side gate.
- **Hook engine** (`BEFORE_INITIALIZE` on the CLMM): a superset — solves this but
  is a large keyspace-changing build. Tracked as a *separate, product-driven*
  decision (programmable graduation policy as a differentiator), **not** required
  to fix M-1/M-2. Out of scope here.

---

## 2. Solution (two complementary layers — each closes a different vector)

The two layers are **not** redundant defense-in-depth for one threat: Layer A
closes M-2, Layer B closes M-1. Both are needed.

### Layer A — Entropy (issuer/seeder only, no shared-infra change)

Make the launch denom and the sink/locker addresses **unpredictable until the
`RegisterLaunch` tx exists** (step 2), by injecting a per-launch salt chosen at
registration. Note this only helps M-2 (sink/locker, created *in*
`RegisterLaunch`); for M-1 the denom becomes public the instant this tx lands, so
the pool remains squattable until `Settle` — Layer B is M-1's fix.

```
subdenom  = {prefix}_{internal_id}_{salt}          // was {prefix}_{internal_id}
sink_salt = encode(issuer, internal_id, salt)      // derives sink + locker addrs
```

- `salt` is chosen by the keeper at registration (step 2) and passed into
  `RegisterLaunch` (and threaded into the `create_sink_payload` salt + locker
  salt).
- Nothing requires these to be predictable in advance: the bank denom is
  internal (the ERC20 is the EVM-facing identity), MTS pairing happens *inside*
  `RegisterLaunch`, and the sink address only needs to reach the EVM forwarder
  (for Leg C) *after* `RegisterLaunch` creates it — the keeper relays it.
- `LAUNCHES` keeps keying on `internal_id`; only the denom string and salts gain
  entropy.

**Effect — and its hard limit:** entropy removes the "read the sequential
counter days ahead" pre-`RegisterLaunch` window for both vectors, but the two
vectors are *not* symmetric:

- **M-2 (fully mitigated):** the sink/locker is created *inside* `RegisterLaunch`
  (step 2). A salt unknowable until that tx is in the mempool reduces M-2 to a
  same-block front-run of `RegisterLaunch` — profitless griefing, defeated by
  retrying with fresh entropy. Attrition favors the defender.
- **M-1 (NOT meaningfully mitigated):** the pool is created at `Settle` (step 5),
  but the launch denom is **public from `RegisterLaunch` (step 2)**. The salt is
  revealed the instant the denom is minted, so an attacker simply reads it from
  the event and squats the pool any time during the curve lifetime before
  `Settle`. Entropy only closes the (pointless) pre-step-2 sub-window and leaves
  the dominant step-2→5 window wide open. **M-1 is closed only by Layer B.**

**Change sites:** `choice_mts_issuer/src/contract.rs` subdenom construction
(~L222) + the `LaunchRecord`; the keeper's salt derivation for `CreateSink` /
`CreateLocker`. Off-chain/backend consumers must read the denom from the
`RegisterLaunch` event rather than recomputing it.

### Layer B — Namespace-scoped creation-authorization gate (Choice CLMM factory)

Close the mempool residual with a generic, launchpad-agnostic gate on
`choice_clmm_factory`. The factory lets the **owner of a tokenfactory denom
namespace** designate who may create the canonical pool for that denom; all other
pairs stay open exactly as today.

**State (additive):**
```rust
// (token0_key, token1_key, fee) -> authorized creator + expiry
pub struct PoolCreationAuth { pub creator: Addr, pub expires_at: u64 }
pub const POOL_CREATION_AUTH: Map<(&str, &str, u32), PoolCreationAuth> =
    Map::new("pool_creation_auth");
```

**Execute (new):**
```rust
AuthorizeCreation { token_a, token_b, fee, creator, ttl_seconds }
CancelCreationAuth { token_a, token_b, fee }
```
Authorization to *set* an entry: `info.sender` must own the tokenfactory
namespace of one side — i.e. one of `token_a`/`token_b` is
`factory/{X}/…` with `X == info.sender`. Pure string parse, no tokenfactory query,
works even before the denom is minted. (Optionally also allow the factory owner,
for non-tokenfactory edge cases.)

**This auth model is already proven in-repo.** The legacy `choice_factory`'s
`execute_add_native_token_decimals` does exactly this parse: for a
`factory/{X}/sub` denom it requires `sender == X` (or the factory owner),
canonicalized, with no tokenfactory query. Layer B is that same check lifted onto
the CLMM factory's `CreatePool`. (Note the CLMM factory carries **no** decimals
registry — unlike the legacy factory — so `POOL_CREATION_AUTH` is the only new
state being added; the gate doesn't need to know or verify denom decimals.)

**Check (added to `execute_create_pool`, after sorting tokens):**
```rust
if let Some(auth) = POOL_CREATION_AUTH.may_load((k0, k1, fee))? {
    if now <= auth.expires_at {
        ensure!(info.sender == auth.creator, "pool slot authorized to another creator");
    }
    POOL_CREATION_AUTH.remove((k0, k1, fee)); // consume / clear expired
}
// else: open creation, identical to today
```

**Issuer wiring:** in `RegisterLaunch`, after `MsgCreateDenom`, the issuer (which
*is* the namespace owner `factory/{issuer}/…`) emits
`AuthorizeCreation { launch_denom, pair_denom, fee, creator: sink_addr, ttl }`
before/with the `CreateSink` forward. At `Settle` the sink's `CreatePool` passes
the check; nobody else can occupy that slot.

**Effect:** pre-creation by a squatter is rejected (not authorized), even in the
mempool-race window. Combined with Layer A's unpredictability, both M-1 and M-2
are closed: the slot is unguessable *and* gated.

---

## 3. Why this is abuse-free

The gate is **namespace-scoped**, so it can never block a pool a graduation
doesn't own:

- Authorization keys include the launch denom, which is **unique per launch**
  (and, with Layer A, unguessable). Authorizing `(launch_denom, pair, fee)`
  blocks a slot no one else would ever want.
- Only the namespace owner can authorize. An attacker cannot
  `AuthorizeCreation` for `(uusdc, inj, 3000)` — they don't own the `uusdc`
  namespace, and a blue-chip pair has no `factory/{attacker}/…` side to match.
- `CreateSink` staying permissionless is fine: a sink an attacker spins up for
  some denom can only ever create the pool for *that* denom's slot, which is
  itself gated to whoever the namespace owner authorized.
- TTL + `CancelCreationAuth` prevent an abandoned launch from locking a slot
  forever (irrelevant for unique launch denoms, but good hygiene + lets you
  re-authorize at a different fee tier).

---

## 4. Decoupling boundary (two projects, one thin interface)

- **Choice CLMM** gains a *generic* primitive: "a tokenfactory namespace owner
  may designate the authorized creator of pools for its denoms." It imports
  nothing about launchpads, the issuer, the seeder, or SHROOM.
- **The on-ramp** (issuer/seeder) is one *consumer* of that primitive. Any EVM
  project plugs in through its own issuer namespace. Swap the launchpad and
  Choice is unchanged.

This keeps the "technically different projects, designed to mesh" boundary clean
and is the correct expression of "we own the venue, so we gate the privileged
graduation path."

---

## 5. Backward compatibility & blast radius

- The `CreatePool` check is **purely additive**: keys with no auth entry behave
  exactly as today. No existing pool, router, or indexer path changes.
- No pool key change (contrast the hook-engine option, which would migrate the
  `(t0,t1,fee)` keyspace). The graduated pool is a normal, routable, indexable
  Choice pool.
- Touches shared infra (`choice_clmm_factory`) on the hot `CreatePool` path —
  one `may_load`. Small, but the factory is load-bearing → needs its own audit
  pass.
- Keep the seeder's `ClmmPoolAlreadyExists` guard as belt-and-suspenders; with
  the gate it should never fire on a legit launch.

---

## 6. Test plan

CLMM factory unit tests:
- open creation unchanged when no auth entry exists;
- authorized creator succeeds; unauthorized `CreatePool` on a gated slot fails;
- only namespace owner may `AuthorizeCreation`; non-owner rejected;
- attacker cannot authorize a slot for a foreign (non-`factory/{self}/`) denom;
- TTL expiry reopens the slot; `CancelCreationAuth` releases early.

Issuer/seeder:
- entropy: two launches with same `internal_id` semantics produce distinct,
  unguessable denoms/salts; `LaunchRecord` round-trips;
- `RegisterLaunch` emits `AuthorizeCreation { creator: sink_addr }` and the
  derived sink address matches the authorized creator;
- end-to-end (test-tube): squatter pre-`CreatePool` is rejected; legit `Settle`
  succeeds; squatter pre-`Instantiate2` of the sink fails to match the
  entropy-derived address.

---

## 7. Sequencing

1. **Layer A (entropy)** — cheap, issuer/seeder only, no shared-infra change.
   Ship first; it closes **M-2** (sink/locker squat) down to a profitless
   same-block race. It does **not** close M-1 — the launch denom is public from
   `RegisterLaunch`, long before the pool is created at `Settle` (see §2).
2. **Layer B (gate)** — Choice factory change + issuer wiring + audit. This is
   **required, not optional**: it is the *only* thing that closes **M-1** across
   the whole product line. Layer A is M-2-only; Layer B is M-1's actual fix.
3. **Hook engine** — *separate* decision: build only if programmable,
   venue-enforced graduation policy (LP locks, anti-snipe, fee routing) is part
   of how "graduate to Choice" out-competes graduating to a permissionless V4.
   The anti-squat `before_initialize` gate falls out of it for free, but is not
   required by this plan.

---

## 8. Resolved decisions

- **Factory-owner authorizer: yes.** `AuthorizeCreation` / `CancelCreationAuth`
  accept *either* a tokenfactory namespace owner of one side *or* the factory
  `config.owner` (governance). Mirrors the legacy factory's "owner OR namespace
  owner" pattern; covers manual/emergency reservations for non-tokenfactory pairs.
- **TTL: `0` means no-expiry** (stored as `expires_at = u64::MAX`). The issuer
  passes `0` for every graduation — the launch denom is unique, so an indefinite
  reservation is harmless, and it can never expire before `Settle` (which may be
  days after `RegisterLaunch`). `CancelCreationAuth` cleans up an abandoned/
  refunded launch. Finite TTLs remain available for the owner/manual path.
- **Salt source: keeper RNG, persisted.** The keeper draws a random per-launch
  salt at the `Observed`→`register` step and persists it on `LaunchRecord` (state
  file) so retries/restarts and address re-derivation are deterministic. Folded
  into the subdenom suffix, the sink `Instantiate2` salt, and the locker salt.

(Dropped: an earlier draft asked whether to require native-denom *existence* in
`CreatePool` as a third layer. It buys nothing here — under the real flow the
launch denom is created at `RegisterLaunch` (step 2) and therefore *exists* for
the whole M-1 window before `Settle`. An existence check would add a bank query
to the hot `CreatePool` path and stop no squat. Layer B is the fix.)

---

## 9. Implementation

Concrete, file-level change set. Decisions from §8 are baked in.

### 9.1 Layer B — `choice_clmm_factory` (shared infra; additive)

**`packages/choice_clmm_common/src/factory.rs`**
- `ExecuteMsg` gains:
  - `AuthorizeCreation { token_a, token_b, fee, creator, ttl_seconds }`
  - `CancelCreationAuth { token_a, token_b, fee }`
- `QueryMsg` gains `GetCreationAuth { token_a, token_b, fee } -> Option<CreationAuthResponse>`.
- New `CreationAuthResponse { creator: String, expires_at: u64 }`.

**`contracts/choice_clmm_factory/src/state.rs`**
```rust
#[cw_serde]
pub struct PoolCreationAuth { pub creator: Addr, pub expires_at: u64 }
pub const POOL_CREATION_AUTH: Map<(&str, &str, u32), PoolCreationAuth> =
    Map::new("pool_creation_auth");
```

**`contracts/choice_clmm_factory/src/contract.rs`**
- `sorted_keys(token_a, token_b)` helper — same sort + `.key()` as `CreatePool`,
  reused by authorize / cancel / create so stored keys are byte-identical.
- `sender_owns_namespace(api, sender, asset)` — parses `factory/{owner}/sub`,
  compares canonicalized; returns false for non-`factory/` denoms / CW20.
- `execute_authorize_creation`: reject same-token; validate CW20 addrs + `creator`;
  require fee tier exists; require `sender_owns_namespace(token0||token1)` **or**
  `sender == config.owner`; `expires_at = if ttl==0 {u64::MAX} else {now+ttl}`; save.
- `execute_cancel_creation_auth`: same auth check; remove (error if absent).
- **Gate** inside `execute_create_pool` (thread `info.sender`, use `env.block.time`),
  after sorting, before the `POOLS.has` check:
  ```rust
  if let Some(a) = POOL_CREATION_AUTH.may_load(deps.storage, (&key0,&key1,fee))? {
      if now <= a.expires_at && info.sender != a.creator {
          return Err(StdError::generic_err("pool slot reserved for another creator"));
      }
      POOL_CREATION_AUTH.remove(deps.storage, (&key0,&key1,fee)); // consume / clear-expired
  }
  ```
- `query_creation_auth` for `GetCreationAuth`.

### 9.2 Issuer wiring — `choice_mts_issuer`

- `Cargo.toml`: add `choice_clmm_common` path dep (reuse the factory message
  schema, same rationale as the existing `choice` dep for `AddNativeTokenDecimals`).
- `msg.rs`: `RegisterLaunch` gains two optional params:
  - `salt_suffix: Option<String>` (Layer A) — alphanumeric, folded into the subdenom.
  - `clmm_pool_auth: Option<ClmmPoolAuth>` where
    `ClmmPoolAuth { clmm_factory: String, fee: u32, ttl_seconds: u64 }` (Layer B).
- `contract.rs`:
  - Subdenom: `format!("{prefix}_{internal_id}")` or `..._{salt_suffix}`; validate
    suffix is ASCII-alphanumeric and that the **final** subdenom ≤ 44 chars.
  - When `clmm_pool_auth` is `Some`, emit (before the `CreateSink` forward) a
    `WasmMsg::Execute` on `clmm_factory`:
    `AuthorizeCreation { token_a: native(launch_denom), token_b: native(pair_denom), fee, creator: seeder_addr, ttl_seconds }`.
    Issuer owns `factory/{issuer}/…`, so the auth passes; `creator = seeder_addr`
    is the sink that runs `CreatePool` at `Settle`. For graduations `ttl_seconds = 0`.

### 9.3 Layer A salts — seeder + keeper (no seeder contract change)

The seeder's `CreateSink` / `CreateLocker` already take a caller-supplied salt, so
only the **keeper** changes: generate a persisted RNG salt and fold it into the
subdenom suffix, the `issuerSalt` (sink) bytes, and the locker salt; read the denom
and sink/locker addresses back from the `RegisterLaunch` event / `Launch` query
rather than recomputing off the sequential counter. *(TS; out of cargo-test scope —
tracked as a follow-up wiring task.)*

### 9.4 Off-chain consumers
Backend/FE/watchdog that reconstruct `factory/{issuer}/{prefix}_{id}` must read the
denom from the `register_launch` event attribute instead. *(Follow-up.)*

### 9.5 Tests (this change set)
- **Factory** (`src/test.rs`): no-entry path unchanged; authorized creator
  succeeds; unauthorized rejected on a gated slot; only namespace owner / factory
  owner may authorize; foreign-denom authorize rejected; `ttl=0` never expires;
  finite TTL expiry reopens; `CancelCreationAuth` releases; key matches `CreatePool`
  under both token orderings; gate consumes the entry.
- **Issuer** (`src/tests.rs`): `clmm_pool_auth: Some` emits `AuthorizeCreation`
  with `creator == seeder_addr` and the right denoms/fee; `None` preserves today's
  message chain; `salt_suffix` changes the denom and is rejected when non-alnum or
  when it overflows the 44-char subdenom cap.
