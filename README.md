# Choice Exchange Smart Contracts

Choice Exchange is an AMM protocol forked from TerraSwap, modernized to work on the Injective blockchain.

0.05% of all swap fees sent straight to the Injective burn auction basket 🔥

[Contracts Documentation](docs/choice.md)

[Testnet Contracts](deployed_contracts.md)

## Security Audit

A security audit was performed by SCV Security and can be found here:

[Audit v1.0](audit/audit_v1.0)

## Main changes

The Choice exchange protocol has extended the contracts of TerraSwap in several ways.

1. Upgraded from cosmwasm v1 to v2.
2. The LP token generated in the pair contract is now a native Injective denom made on the token factory module.
3. The factory contract takes 2 additional parameters: burn_address and fee_wallet_address

### Burn wallet address

The burn wallet address refers to a custom contract `choice_send_to_auction` which accepts both cw20 and native denoms. This contract sends the funds to the Injective burn action basket.

### Fee wallet address

The fee wallet address is a wallet where a part of the swap fee is sent.

## Development

`cargo build`
`cargo test`

## Build

For a production-ready (compressed) build, run the following from the repository root:

```bash
docker run --rm -v "$(pwd)":/code \
  --mount type=volume,source="$(basename "$(pwd)")_cache",target=/code/target \
  --mount type=volume,source=registry_cache,target=/usr/local/cargo/registry \
  cosmwasm/workspace-optimizer:0.16.1
```

The optimized contracts are generated in the artifacts/ directory.

## Deploy to testnet example

Set your injective cli configuration variables in the deploy script file:

[deploy_testnet.sh](testnet_deploy/deploy_testnet.sh)