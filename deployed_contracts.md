# Deployed Contracts

## Testnet Deployment

## Code IDs

| Component      | Code ID |
| -------------- | ------- |
| Pair           | 31996   |
| Factory        | 31997   |
| Burn Manager   | 31998   |
| CW20 Adapter   | 31999   |
| Router         | 32000   |
| Zap LP (v2.0.0)| 39459   |

## Contract Addresses

| Component      | Address                                                           |
| -------------- | ----------------------------------------------------------------- |
| CW20 Adapter   | `inj1m2shmqwjs39ummgteafpdfcsmlc7395rk2hfdv`                        |
| Burn Manager   | `inj1f552m9nfc7ae9c3pc4jhmvpcgjhlkw3tgqd92v`                        |
| Factory        | `inj18egg5e9p0k2wn03s0vn6k87xfgcpmfq2fzhugp`                        |
| Router         | `inj1j62jw77t0fk54rq8m5ztk4apu86tawcjgufp7t`                        |
| Zap LP (UI)    | `inj1zekjv7tsge94da5kxrhds70kvnv9pqc39mtvt4`                        |

### Testnet zap-LP royalty streams

One contract instance per `(input, pair)` royalty stream. All pinned to pair
`inj1w0l60dw4c8kp73k6wda0ns04hcts37l7nj879r` (CW20 ↔ INJ).

| Stream                           | Address                                      |
| -------------------------------- | -------------------------------------------- |
| CW20 royalty (`inj17qld…m69xl7`) | `inj1jhkl8uyk4qp8sa5xmfvxdklyk6g3zuszv2aht5` |
| INJ royalty (`native:inj`)       | `inj1s3dfkxt4lx0lcsn3k2wkazvwuxfupzd707dmw5` |

Deprecated v1 zap-LP contract `inj1crke0ye0jhace2eryy4mvlna3qluqgyaz4v0md`
(code id 39458) is neutralized: `default_recipient` cleared, all keepers
removed. Do not direct new royalty flows at it.

### Testnet CLMM (fresh deploy 2026-06-11, admin `inj1q2m26a7j…`)

Code IDs: pool `39611`, factory `39612`, manager `39613`.

- **CLMM Factory:** `inj1tscqujw9j9sa9n49xzln0f0n96gvy32dq5gw3c` (`pool_code_id` 39611; fee tiers 100/500/3000/10000)
- **CLMM Manager (cw721 positions):** `inj1srtw4f8yzf4dvnmqr02n7qhpt2px0zf0v9lz5e`

## Additional Addresses

- **Fee Wallet Address:** `inj1nwk46lyvhmdj5hr8ynwdvz0jaa4men9ce2gt58`
- **Admin Address:** `inj1q2m26a7jdzjyfdn545vqsude3zwwtfrdap5jgz`

## Testnet dApp

You can interact with the contracts at https://testnet.choice.exchange (code `letmein`)

## Mainnet 

Choice Treasury Multisig: `inj1c2yleauy9say73tsx3dk5tvlgwwzdh96r76zv4`

Choice Dev Multisig: `inj1vcszz8j58m79exzdlpa8m9u5eyu9r37u7jhm7k`

## Mainnet Code IDs

| Component         | Code ID |
| ----------------- | ------- |
| Pair              | 1692    |
| Factory           | 1693    |
| Burn Manager      | 1690    |
| Router            | 1691    |
| Admin Timelock    | 1999    |
| Farm              | 2015    |
| Farm Factory      | 2016    |
| Zap LP            | 2002    |
| Token Locker      | 2003    |
| CLMM Pool         | 2039    |
| CLMM Factory      | 2040    |
| CLMM Manager      | 2041    |
| DEX Aggregator v2 | 2042    |

Farm + Farm Factory were redeployed 2026-05-24 to ship the `AddSchedules`
exec on `choice_farm`. The previous code ids (Farm `2000`, Farm Factory
`2001`) remain live for already-spawned farms but are superseded for new
launches — frontend points at the new factory only.

## Mainnet Contract Addresses

| Component         | Address                                      |
| ----------------- | -------------------------------------------- |
| CW20 Adapter      | `inj14ejqjyq8um4p3xfqj74yld5waqljf88f9eneuk` |
| Burn Manager      | `inj1yr7srge0lku4h3gd473qdlpdfw63ejdjwkh4c0` |
| Factory           | `inj1k9lcqtn3y92h4t3tdsu7z8qx292mhxhgsssmxg` |
| Router            | `inj1ne2durmsx2jurvy4wgnhegv3xt6789up8xgum3` |
| Admin Timelock    | `inj14tm9kjh396g483aj76xyykem2mdk22q8x769v9` |
| Farm Factory      | `inj1v79393jctqw38jpwukwuwelwups7qmxtpxzd75` |
| Zap LP            | `inj17tvqalm2u06a7vjpn8p62czukzyvy8m07sf6c5` |
| Token Locker      | `inj1y5gtmlv695jz2s5q2lqq0l3h34040nh32snv4m` |
| CLMM Factory      | `inj1k58yqvsww97asl6eajx0lja6np03naddfc6cs7` |
| CLMM Manager      | `inj1eag2kjzs5ma5sflxvlhaacdxxpvdjg4ny7yg3g` |
| DEX Aggregator v2 | `inj1520rsss9aykhkfmuf89nh5hp2jww770z4u3eu0` |

Individual farms are spawned by Farm Factory (code id 2015); their addresses are
emitted in the `CreateFarm` tx events and not tracked centrally here.

The `Zap LP` row above (`inj17tvqalm…`) is the lazy UI instance (`input`/`pair`
unset). Dedicated royalty-stream instances are listed below.

### Mainnet zap-LP royalty streams

One contract instance per `(input, pair)` royalty stream (code id 2002, v2.0.0).
`owner` and wasm-admin are the Dev Multisig; LP + dust go to `default_recipient`.

**INJ royalty → INJ/USDC** (deployed 2026-06-03)

- Contract: `inj1srnm0dzys640j8t75tc9l37aen6m6lagqfzzct`
- Input: `native:inj` · Pair: `inj1583lyh82kgflpwe25lsvsmw5t343hawyef5ppw` (INJ/USDC)
- `default_recipient`: Treasury Multisig `inj1c2yleauy9say73tsx3dk5tvlgwwzdh96r76zv4`
- `tip_bps`: 25 · `min_zap_amount`: 1 INJ (`1000000000000000000`)
- Keeper: `choice-zap-keeper` on the `choice` host (`/root/bots/choice-zap-keeper`,
  pm2). Keeper hot key `inj1wt8yrdajp6h65qedunmurqv9nypzlm839eck66` (allowlisted via
  `AddKeeper`). Point INJ royalties at the contract via `MsgSend`.

### Mainnet CLMM + Aggregator v2 (deployed 2026-06-12)

Deployed via `deploy/deploy_clmm_mainnet.sh` + `aggregation_contract/scripts/{upload_code,deploy}_mainnet.sh`. Results: `deploy/results/mainnet_clmm_20260612_144113.json`.

- **CLMM Factory:** `inj1k58yqvsww97asl6eajx0lja6np03naddfc6cs7` (`pool_code_id` 2039; default fee tiers 100/500/3000/10000). owner = EOA `inj1q2m26a7j…` (multisig rotation DEFERRED — propose+accept when signers online). wasm-admin = Admin Timelock.
- **CLMM Manager (cw721 positions):** `inj1eag2kjzs5ma5sflxvlhaacdxxpvdjg4ny7yg3g`. wasm-admin = Admin Timelock.
- **DEX Aggregator v2** (orderbook-merge + FlashRoute, CLMM-capable): `inj1520rsss9aykhkfmuf89nh5hp2jww770z4u3eu0` (code 2042). config.admin (flash-signer gate, PERMANENT) = ops EOA `inj1yrg4pg8hcu…`; wasm-admin = Admin Timelock. Supersedes legacy agg `inj1a4qvqym…` for CLMM/new-shape routing.
- **Pools:** none yet — created permissionlessly via the FE (fee-free; pool protocol-carve default OFF). Stores cost ~4.1M gas (CLMM_GAS 8M override).
- **Flash:** deny-all until BOTH gates open — aggregator `AuthorizeFlashSigner{signer:<bot>}` (config.admin) AND factory `AuthorizeFlashBorrower{borrower:<agg>}` (owner).

### Superseded mainnet factories (not in FE)

| Component       | Address                                      | Notes                                                                                                                                                                          |
| --------------- | -------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| Farm Factory v1 | `inj1xweu85c0uds9fqg0tq4s69vxamgsvjt63x9v4x` | code id 2001, farm code id 2000. Deployed 2026-05-14. Still spawning/serving the farms it created; FE no longer surfaces it. New launches go via the 2026-05-24 redeploy above. |

### Mainnet ownership & wasm-admins (post-deploy 2026-05-14)

Deployed via `deploy/deploy_lazy_full.sh`, results in
`deploy/results/mainnet_20260515_094951.json`.

| Contract       | wasm-admin     | config.owner / admin     | Notes                                       |
| -------------- | -------------- | ------------------------ | ------------------------------------------- |
| Admin Timelock | Dev Multisig   | EOA `inj1q2m26a…` (lazy) | 48h propose+apply pending → Dev Multisig    |
| Farm Factory   | Admin Timelock | EOA `inj1q2m26a…` (lazy) | 48h propose+apply pending → Dev Multisig    |
| Token Locker   | Dev Multisig   | Dev Multisig (instant)   | H-2 verified wasm-admin == config.admin     |
| Zap LP         | Dev Multisig   | Dev Multisig (instant)   | `input`/`pair` unset; wire via UpdateConfig |

Two deferred 48h-timelocked rotations of `config.owner` (Admin Timelock and
Farm Factory) remain on the EOA pending `propose_new_owner` → wait → `apply_owner_rotation`.
See [README.md](deploy/README.md) (if present) or the script's tail output for the exact
`injectived tx wasm execute` commands.

## Misc

| Label         | Address                                      |
| ------------- | -------------------------------------------- |
| Fee Wallet    | `inj1c2yleauy9say73tsx3dk5tvlgwwzdh96r76zv4` |
| Admin Address | `inj1q2m26a7jdzjyfdn545vqsude3zwwtfrdap5jgz` |
