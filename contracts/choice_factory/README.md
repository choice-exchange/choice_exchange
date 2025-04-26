
# Choice Factory

The factory contract can create choice pair contracts and also act as a directory for all pairs. The sender of the instantiation message becomes the owner of the factory contract.

## InstantiateMsg

This message registers the verified pair contract for subsequent pair creation. In addition to specifying the code ID for the pair contract, you also provide:
- **burn_address:** The address of the send_to_auction contract.
- **fee_wallet_address:** The address where fees will be collected.

Example:

```json
{
  "pair_code_id": 123,
  "burn_address": "inj1abc...xyz",
  "fee_wallet_address": "inj1def...uvw"
}
```

## ExecuteMsg

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

### `ProposeNewOwner`

Stage a transfer of factory ownership. Only the **current owner** may call this.

```json
{
  "propose_new_owner": {
    "new_owner": "inj1…newOwnerAddress"
  }
}
```

### `AcceptOwnership`

Called by the **proposed owner** to finalize and accept the transfer.

```json
{
  "accept_ownership": {}
}
```

### `CancelOwnershipProposal`

Allows the **current owner** to clear any pending ownership proposal before it’s accepted.

```json
{
  "cancel_ownership_proposal": {}
}
```

---

### `UpdateConfig`

Owner-only. Atomically update pair‐creation settings—**no longer** used to change owner:

```json
{
  "update_config": {
    "pair_code_id": 456,                          // optional; leave null to keep old
    "burn_address": "inj1…newBurnAuctionAddr",    // optional
    "fee_wallet_address": "inj1…newFeeWalletAddr" // optional
  }
}
```

- `pair_code_id`: new code ID for newly instantiated pair contracts  
- `burn_address`: address of your send_to_auction contract  
- `fee_wallet_address`: address where swap fees are collected  

Any field set to `null` remains unchanged.  

---

All of these new messages are gated by the existing owner check & two-step transfer logic, ensuring only the rightful owner can propose, cancel, or accept ownership, and only that owner can update factory settings.

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
