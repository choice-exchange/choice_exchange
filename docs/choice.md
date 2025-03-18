# Choice Exchange

**Choice Exchange** is an AMM protocol forked from Terraswap. It has been updated to use the latest CosmWasm libraries and tailored specifically for Injective. In addition to the core swap functionality, Choice adds a unique fee distribution mechanism via the **send_to_auction** contract.

## Contract Overview

### Factory Contract
- **Purpose:**  
  Creates new pair contracts (liquidity pools) and acts as a directory for all pairs.
- **Key Parameters:**  
  - `pair_code_id`: Code ID for the pair contract.
  - `token_code_id`: Code ID for the token contract used in pair creation.
  - `burn_address`: The deployed **send_to_auction** contract address.
  - `fee_wallet_address`: Address where 0.05% of swap fees are sent (Choice fee wallet).
- **Token Registration:**  
  Registers native tokens (including IBC tokens) with their decimals. The contract must hold at least 1 token to verify decimals. CW20 tokens must be registered via the CW20 adapter contract for swaps to work.

### Pair Contract
- **Purpose:**  
  Manages liquidity pools and swap operations.
- **LP Tokens:**  
  LP tokens are created as native Injective denominations using the token factory module.  
  - **Fee:** A fee is charged for pair creation (1 INJ on testnet; 0.1 INJ on mainnet).  
  - **LP Denom Format:** If the pair contract is `inj123`, then the LP token denom will be `factory/inj123/lp`.
- **Advantage:**  
  Using native LP denoms eliminates the need for deploying a separate CW20 token contract for liquidity provision.

### Router Contract
- **Purpose:**  
  Provides simulation and execution of swap operations by interacting with the factory and pair contracts.

### Send_to_auction Contract
- **Purpose:**  
  A new addition in Choice that leverages the Injective CW20 adapter to send 0.05% of swap fees to the Injective burn action basket (or subaccount).  
- **Fee Distribution:**  
  This mechanism ensures that part of the swap fees are sent to the burn action basket, supporting the ecosystem's tokenomics.

## Deployment Steps

1. **Upload Contracts:**  
   Upload the compiled binaries for the **factory**, **router**, **pair**, and **send_to_auction** contracts to the network and record their code IDs.

2. **Instantiate Send_to_auction:**  
   Deploy the send_to_auction contract first.

3. **Instantiate Factory:**  
   When instantiating the factory contract, use:
   - The send_to_auction contract address as the `burn_address`
   - Your designated fee wallet address as `fee_wallet_address`

4. **Instantiate Router:**  
   Provide the factory contract’s address during the router instantiation.

5. **Register Token Denoms:**  
   For each native token (including IBC tokens), send 1 unit of the token to the factory contract and invoke `add_native_token_decimals` to register its decimals.

6. **Create a New Pair:**  
   Use the factory contract to create a new pair.  
   The pair contract will automatically generate the LP token as a native Injective denom (format: `factory/{pair_contract_address}/lp`).

7. **Add Liquidity & Start Swapping:**  
   Once a pair is created, add liquidity and perform swaps via the router contract.
