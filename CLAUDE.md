# Choice Exchange

CosmWasm v2 smart contracts for a DEX on Injective. Two AMM systems: legacy constant-product (choice_pair/factory) and concentrated liquidity CLMM (choice_clmm_pool/factory/manager).

## Build & Test

```bash
cargo build                         # compile all contracts
cargo test                          # unit tests
make build-all                      # build CLMM WASM artifacts to artifacts/
make build-factory                  # build choice_clmm_factory only
make build-pool                     # build choice_clmm_pool only
make build-manager                  # build choice_clmm_manager only
make test                           # build-all + integration tests
cargo test --test integration       # integration tests (requires artifacts/)
./build_release.sh                  # Docker optimized production build
```

Integration tests use `injective_test_tube` and require compiled WASM in `artifacts/` — run `make build-all` before `cargo test --test integration`.

## Detailed Docs

Read these for deeper context on specific contract systems:

- `docs/choice.md` — Legacy AMM: pair swap math, fee splits, factory, router, send_to_auction
- `docs/choice_clmm.md` — CLMM: pool actions, fee accounting, tick bitmap, oracle, math library, factory, manager
- `docs/farm.md` — Farm: reward index pattern, distribution schedules, bond/unbond/withdraw
- `docs/vault.md` — Vault: two-phase deposits, share model, auto-compound reply chain, reward routing

## Project Structure

```text
contracts/
  choice_clmm_pool/       # CLMM concentrated liquidity pool (mint, swap, burn, collect)
  choice_clmm_factory/    # Creates CLMM pools with fee tiers
  choice_clmm_manager/    # NFT position manager for CLMM (cw721)
  choice_pair/            # Legacy XYK constant-product pair
  choice_factory/         # Legacy pair factory
  choice_router/          # Multi-hop swap routing
  choice_farm/            # LP staking and reward distribution
  choice_farm_factory/    # Spawns farms; registry + INJ launch fee
  choice_admin_timelock/  # Holds wasm-admin powers; delays MsgMigrateContract
  choice_vault/           # Auto-compounding vault
  choice_send_to_auction/ # Sends 0.05% swap fees to Injective burn auction
packages/
  choice/                 # Shared types for legacy contracts (Asset, PairInfo, etc.)
  choice_clmm_math/       # CLMM math: Q64.96 sqrt prices, tick math, swap math, liquidity math
  choice_clmm_common/     # Shared types for CLMM contracts (messages, pool state)
tests/                    # Integration tests (integration.rs, visualization.rs)
artifacts/                # Built WASM binaries
```

## Code Conventions

**Contract layout**: `contract.rs` (entry points) / `state.rs` (storage) / `error.rs` (errors) / `actions/` (execute handlers) / `core/` (algorithms)

**Naming**: PascalCase types/enums, snake_case functions/modules, UPPER_CASE storage constants

**Error handling**: Custom `ContractError` enum with `#[from] StdError`. Use checked arithmetic (`.checked_add()`, `.checked_sub()`) with `.ok_or()` for overflow protection. Validate inputs early.

**Storage**: `Item<T>` for singletons, `Map<K, V>` for collections (cw_storage_plus). Storage constant names match their string keys (e.g., `POOL_CONFIG: Item<PoolConfig> = Item::new("pool_config")`).

**Formatting**: 4-space indent, unix newlines. See `rustfmt.toml`.

## Architecture

- CLMM follows Uniswap V3: tick-based concentrated liquidity, tick bitmap for efficient traversal, "outside model" for fee accumulation
- CLMM supports both native tokens and CW20 tokens via `AssetInfo` enum (`NativeToken { denom }` | `Token { contract_addr }`). Pools can be any combination (native/native, native/CW20, CW20/CW20). CW20 swaps work via `Receive` hook (CW20 Send) or `TransferFrom` (requires allowance)
- Prices stored as Q64.96 fixed-point sqrt prices (`Uint256`). Ticks are `log_1.0001(price)` integers in range `[-887272, 887272]`
- Fee growth accumulators use wrapping U256 arithmetic (intentional overflow at `U256::MAX`)
- Dynamic fees: EMA price oracle adjusts fee between `base_fee_ppm` and `max_fee_ppm` based on volatility
- CLMM positions are NFTs minted by choice_clmm_manager (cw721)
- Legacy LP tokens are native Injective denoms: `factory/{pair_address}/lp`

## Key Dependencies

- `cosmwasm-std` 2.2.2, `cw-storage-plus` 2.0.0, `cw2` 2.0.0
- `injective-cosmwasm` 0.3.4-1, `injective-math` 0.3.4-1
- `cw20` 2.0.0, `cw721` / `cw721-base` 0.20.0
- `injective-test-tube` 1.16.3-1 (tests)

## Testing

- **Unit tests**: `cosmwasm_std::testing` mocks in each contract's `tests.rs`
- **Integration tests**: `injective_test_tube` blockchain simulator in `tests/integration.rs`
- **Visualization tests**: `tests/visualization.rs` generates SVG liquidity curve plots
- Test helpers: `TestEnv` struct in integration tests handles app setup, account creation, contract deployment
