# Choice Pair Contract

The Choice Pair Contract is a core component of the Choice protocol, facilitating liquidity pools and swaps between two assets on the Injective blockchain. It is designed to handle liquidity provision, swaps, and queries while enforcing strict validations (such as deadlines and slippage tolerances) and integrating with Injective-specific modules.

---

## Table of Contents

- [Overview](#overview)
- [Handlers](#handlers)
  - [Initialization / Instantiate](#initialization--instantiate)
  - [Liquidity Provision](#liquidity-provision)
    - [Provide Liquidity](#provide-liquidity)
    - [Withdraw Liquidity](#withdraw-liquidity)
    - [Parameters: Receiver, Min Assets, Deadline](#parameters-receiver-min-assets-deadline)
  - [Swap](#swap)
    - [Swap Mechanism and Fee Distribution](#swap-mechanism-and-fee-distribution)
- [Query Endpoints](#query-endpoints)
- [Migration](#migration)
- [Error Handling and Validations](#error-handling-and-validations)
- [Example Request Formats](#example-request-formats)

---

## Overview

The Choice Pair Contract enables users to:
- **Initialize a new pair:** Creating liquidity token contracts and storing pair information.
- **Provide liquidity:** Users deposit two assets into the pool, receiving liquidity tokens representing their share.
- **Swap assets:** Users can swap between assets using a Uniswap-style constant product formula, while fees are split among liquidity providers, a fee wallet, and token burning.
- **Query pool and simulation data:** Retrieve up-to-date pool balances, pair info, and simulate swap results.

This contract is built for the Injective blockchain, leveraging custom query and message wrappers (e.g., `InjectiveQueryWrapper` and `InjectiveMsgWrapper`) and specific modules from `injective_cosmwasm`.

---

## Handlers

### Initialization / Instantiate

The **instantiate** function is primarily called from the Choice Factory contract to set up a new pair. Key actions include:

- **Liquidity Token Creation:**  
  A liquidity token denomination is generated by combining the contract address with a fixed subdenom (`"lp"`). Two messages are sent:
  - **Create New Denom:** Establishes the new liquidity token using the Injective token factory module.  
  This process costs a fee in $INJ tokens depending on what the current fee is set to on chain
  - **Set Token Metadata:** Configures token metadata (name, symbol, decimals).

   

- **Storing Pair Information:**  
  The contract converts provided asset infos to a raw format and saves the pair configuration, which includes:
  - Asset infos and decimals.
  - Burn address (for fee burning).
  - Fee wallet address (receives part of the commission).
  
- **Contract Versioning:**  
  The contract version is stored for migration and compatibility checks.

**Instantiate Message Structure Example:**

```rust
pub struct InstantiateMsg {
    /// Asset infos for the pair.
    pub asset_infos: [AssetInfo; 2],
    /// Decimals for each asset.
    pub asset_decimals: [u8; 2],
    /// Address used for send_to_auction contract (fee burning).
    pub burn_address: String,
    /// Address for the fee wallet (receives commission fees).
    pub fee_wallet_address: String,
}
```

---

### Liquidity Provision

The contract supports two main liquidity-related operations: providing liquidity and withdrawing liquidity.

#### Provide Liquidity

When a user provides liquidity:
- **Depositing Assets:**  
  The user sends two assets (one for each pool). The contract validates the provided amounts and the native token balances.
  
- **LP Token Calculation:**  
  - **Initial Pool:**  
    When the pool is created, the initial share is computed as the square root of the product of the two deposits. A minimum liquidity amount (e.g., 1,000 units) is minted and permanently locked to safeguard the pool.
  - **Existing Pool:**  
    For subsequent deposits, the share is calculated in proportion to the current pool balance using a ratio of the deposits to the existing pool amounts.

- **Refunds and Slippage:**  
  If tokens are sent at a rate different from the current pool ratio, the excess amount is refunded. Users can specify a slippage tolerance to limit how much discrepancy is acceptable.

#### Withdraw Liquidity

To withdraw liquidity:
- **Burn LP Tokens:**  
  The user burns their liquidity tokens to receive back the underlying assets.
- **Minimum Assets:**  
  Optionally, users can specify `min_assets` to ensure that the returned assets are not below a desired threshold.
  
#### Parameters: Receiver, Min Assets, Deadline

- **Receiver:**  
  The `receiver` parameter in `provide_liquidity` allows the user to designate a different address to receive the minted LP tokens. By default, LP tokens are sent to the sender.
  
- **Min Assets:**  
  In `withdraw_liquidity`, if the user sets `min_assets`, the operation is restricted if the assets returned are less than the specified minimums.
  
- **Deadline:**  
  A deadline parameter is provided to ensure that transactions do not execute after a given timestamp. This helps to avoid unexpected market movements and limits transaction validity.

---

### Swap

The swap functionality allows any user to exchange assets, subject to certain constraints.

#### Swap Mechanism and Fee Distribution

- **Swap Types:**  
  - **Native Token → Token Swap:**  
    Direct swap using native tokens.
  - **Token → Native Token Swap:**  
    Performed via a CW20 receive hook where the token contract sends the asset along with a swap message.

- **Constant Product Formula:**  
  The swap computation follows a Uniswap-like constant product model:
  - The return amount is calculated based on the ratio of the pools before and after the offer.
  - **Spread Calculation:**  
    The difference between the expected return (based on the oracle or ratio) and the actual return is computed as the spread.

- **Fee Distribution:**  
  A fixed total fee of **0.3%** is applied on each swap and split as follows:
  - **Liquidity Provider (LP) Commission (0.2%):**  
    Remains in the pool, increasing the overall constant product and benefiting liquidity providers.
  - **Fee Wallet Commission (0.05%):**  
    Transferred to a designated fee wallet.
  - **Burn Amount (0.05%):**  
    Tokens are sent to the burn auction sub account via the choice_sent_to_auction contract

- **Validation:**  
  The contract ensures that:
  - Only native tokens can be directly swapped.
  - The specified slippage and belief price (if provided) do not cause the swap to execute under unfavorable conditions.

---

## Query Endpoints

The contract exposes several query endpoints to help users and integrators retrieve current state and simulation data:

- **Pair Info:**  
  Returns the normalized pair configuration, including asset infos and pool details.

- **Pool:**  
  Retrieves current balances for both assets and the total supply of liquidity tokens.

- **Simulation:**  
  Given an offer asset, it simulates the swap, returning:
  - Expected return amount.
  - Calculated spread.
  - Commission amount.

- **Reverse Simulation:**  
  Calculates the required offer amount for a desired ask asset amount, along with the associated spread and commission.

---

## Migration

The contract includes a migration endpoint (`migrate`) to update its internal version. This process ensures that newer contract versions remain compatible with existing deployments. The target version is specified (e.g., `"0.1.1"`), and the migration function performs version checks and necessary state transformations.

---

## Error Handling and Validations

The contract makes extensive use of custom error types (`ContractError`) to handle failure modes clearly. Key validations include:

- **Deadline Validation:**  
  Transactions are rejected if attempted after the specified deadline.

- **Minimum Asset Check:**  
  Withdrawals validate that the returned assets meet or exceed any user-defined minimums.

- **Unauthorized Actions:**  
  Only approved asset contracts can trigger specific functions (e.g., CW20 receive hooks for token swaps).

- **Slippage and Spread Assertions:**  
  If the computed spread exceeds the user-defined maximum, the swap is aborted.

---

## Example Request Formats

### Provide Liquidity

```json
{
  "provide_liquidity": {
    "assets": [
      {
        "info": {
          "token": {
            "contract_addr": "inj1exampletokenaddress..."
          }
        },
        "amount": "1000000"
      },
      {
        "info": {
          "native_token": {
            "denom": "inj"
          }
        },
        "amount": "1000000"
      }
    ],
    "receiver": "inj1receiveraddress...",
    "deadline": 1680000000,
    "slippage_tolerance": "0.01"
  }
}
```

### Withdraw Liquidity

1. **With Minimum Assets:**

```json
{
  "withdraw_liquidity": {
    "min_assets": [
      {
        "info": {
          "token": {
            "contract_addr": "inj1exampletokenaddress..."
          }
        },
        "amount": "1000000"
      },
      {
        "info": {
          "native_token": {
            "denom": "inj"
          }
        },
        "amount": "1000000"
      }
    ],
    "deadline": 1680000000
  }
}
```

2. **Without Minimum Assets:**

```json
{
  "withdraw_liquidity": {
    "deadline": 1680000000
  }
}
```

### Swap

#### Native Token → Token Swap

```json
{
  "swap": {
    "offer_asset": {
      "info": {
        "native_token": {
          "denom": "inj"
        }
      },
      "amount": "1000000"
    },
    "belief_price": "1.23",
    "max_spread": "0.005",
    "to": "inj1recipientaddress..."
  }
}
```

#### Token → Native Token Swap

*(This message must be sent via the token contract using the CW20 `send` method with an embedded swap message.)*

```json
{
  "send": {
    "contract": "inj1paircontractaddress...",
    "amount": "1000000",
    "msg": {
      "swap": {
        "belief_price": "1.23",
        "max_spread": "0.005",
        "to": "inj1recipientaddress..."
      }
    }
  }
}
```

