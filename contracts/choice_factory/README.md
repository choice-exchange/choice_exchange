
# Choice Factory

The factory contract can create choice pair contracts and also act as a directory for all pairs. The sender of the instantiation message becomes the owner of the factory contract.

## InstantiateMsg

This message registers the verified pair contract and token contract for subsequent pair creation. In addition to specifying the code IDs for the pair and token contracts, you also provide:
- **burn_address:** The address of the send_to_auction contract.
- **fee_wallet_address:** The address where fees will be collected.

Example:

```json
{
  "pair_code_id": 123,
  "token_code_id": 123,
  "burn_address": "inj1abc...xyz",
  "fee_wallet_address": "inj1def...uvw"
}
```

## ExecuteMsg

### `update_config`
Change the factory contract's owner and relevant code IDs for future pair contract creation. This execution is only permitted to the factory contract owner.

```json
{
  "update_config": {
    "owner": "inj...",
    "token_id": 123,
    "pair_code_id": 123,
    "burn_address": "inj1abc...xyz",
    "fee_wallet_address": "inj1def...uvw"
  }
}
```

### `create_pair`
When a user executes `CreatePair` operation, it creates `Pair` contract and `LP(liquidity provider)` token denom using the Injective token factory module.

Injective has a fee which is set on chain. Currently 1 INJ testnet and 0.1 INJ mainnet.

The pair contract is the lp denom owner. Therefore if the pair contract is `inj123` then the lp denom is `factory/inj123/lp`

In order to create pairs with native tokens, including IBC tokens, they must first be registered with their decimals by the factory contract owner. See [add_native_token_decimals](#add_native_token_decimals) for more details.

```json
{
  "create_pair": {
    "assets": [
      {
        "info": {
          "token": {
            "contract_addr": "inj..."
          }
        },
        "amount": "0"
      },
      {
        "info": {
          "native_token": {
            "denom": "inj"
          }
        },
        "amount": "0"
      }
    ]
  }
}
```

### `add_native_token_decimals`

This operation is allowed only for the factory contract owner and registers native tokens (including IBC tokens) along with their decimals.

When a new pair is created and includes a token that was registered via this operation, the contract will automatically use the provided token information to create the pair.

**Note:** The contract must hold at least 1 unit of the token in its balance to verify the token’s decimals.

Additionally, token factory creators can use this function to register the decimals for tokens they have created.

```json
{
  "add_native_token_decimals": {
    "denom": "inj",
    "decimals": 6
  }
}
```

### `migrate_pair`

```json
{
  "migrate_pair": {
    "contract": "inj...",
    "code_id": 123
  }
}
```

## QueryMsg

### `config`

```json
{
  "config": {}
}
```

### `pair`

```json
{
  "pair": {
    "asset_infos": [
      {
        "token": {
          "contract_addr": "inj..."
        }
      },
      {
        "native_token": {
          "denom": "inj"
        }
      }
    ]
  }
}
```

### `pairs`

```json
{
  "pairs": {
    "start_after": [
      {
        "token": {
          "contract_addr": "inj..."
        }
      },
      {
        "native_token": {
          "denom": "inj"
        }
      }
    ],
    "limit": 10
  }
}
```

### `native_token_decimals`
```json
{
  "native_token_decimals": {
    "denom": "inj"
  }
}
```
