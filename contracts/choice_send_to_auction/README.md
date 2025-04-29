# Send to Injective Burn Auction Contract

This repository contains a CosmWasm smart contract that facilitates sending tokens (both native and CW20) to the Injective burn auction subaccount. The contract provides an interface for handling native token transfers and CW20 token operations, ensuring proper routing to the designated burn auction subaccount.

The contract requires the contract address of the INJ CW20 Adapter contract.  
Source code: https://github.com/InjectiveLabs/cw20-adapter

---

## Features

1. **Native Token Transfers**:
   - Accepts native tokens and routes them to the Injective burn auction subaccount.
   - Ensures proper validation of funds.

2. **CW20 Token Handling**:
   - Accepts `send` messages from CW20 contracts.
   - Converts CW20 tokens into a token factory denomination and sends them to the burn auction.

3. **Ownership Management**:
   - Supports a secure two-step ownership transfer mechanism:
     - **ProposeNewOwner**: Owner proposes a new owner.
     - **AcceptOwnership**: Proposed owner must accept ownership.
     - **CancelOwnershipProposal**: Owner can cancel a pending ownership proposal.

4. **Admin Management**:
   - Allows updating the contract configuration (such as CW20 adapter contract) via admin-only operations.

5. **Configurable**:
   - The contract's configuration includes the owner address, the CW20 adapter contract, and the burn auction subaccount.

---

## Messages

### InstantiateMsg
Used to initialize the contract during deployment.

```json
{
  "owner": "injective_address_of_owner",
  "adapter_contract": "injective_address_of_cw20_adapter",
  "burn_auction_subaccount": "0x1111111111111111111111111111111111111111111111111111111111111111"
}
```

- `owner`: The initial owner address for managing the contract.
- `adapter_contract`: The address of the CW20 adapter contract.
- `burn_auction_subaccount`: The subaccount ID for the Injective burn auction.

> **Note:**  
> The Injective burn auction subaccount ID is:  
> `0x1111111111111111111111111111111111111111111111111111111111111111`

---

### ExecuteMsg
The main entry point for executing contract actions.

---

#### `SendNative`
Sends native tokens to the burn auction.

```json
{
  "send_native": {
    "asset": {
      "info": {
        "native_token": {
          "denom": "denomination"
        }
      },
      "amount": "amount_in_wei"
    }
  }
}
```

---

#### `Receive`
Handles CW20 tokens sent via a `send` message from a CW20 contract.

```json
{
  "receive": {
    "sender": "cw20_sender_address",
    "amount": "amount",
    "msg": "{}"
  }
}
```

---

#### `ProposeNewOwner`
Proposes a new owner for the contract. Only the current owner can propose.

```json
{
  "propose_new_owner": {
    "new_owner": "new_owner_address"
  }
}
```

---

#### `AcceptOwnership`
Called by the proposed owner to accept and finalize the ownership transfer.

```json
{
  "accept_ownership": {}
}
```

---

#### `CancelOwnershipProposal`
Allows the current owner to cancel a pending ownership proposal.

```json
{
  "cancel_ownership_proposal": {}
}
```

---

#### `UpdateConfig`
Owner-only. Atomically update the CW20 adapter contract address and/or the burn auction subaccount. Any field set to `null` is left unchanged.

```json
{
  "update_config": {
    "adapter_contract": "new_adapter_contract_address",     // optional; omit or null to leave unchanged
    "burn_auction_subaccount": "0xAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA" // optional; omit or null to leave unchanged
  }
}
```

- `adapter_contract`: (Optional) New Injective address of the CW20 adapter.  
- `burn_auction_subaccount`: (Optional) New 0x-prefixed, 64-hex-character subaccount ID.  

All updates are gated by an owner check.