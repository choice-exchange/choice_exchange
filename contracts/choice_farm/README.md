# Staking Contract

This staking contract is forked from the Anchor protocol. It enables users to bond tokens (either native or CW20) in order to earn rewards over time according to a predefined distribution schedule.

## Key Features

- **Bonding & Unbonding:**  
  Users can bond tokens to participate in staking and unbond when desired. The contract tracks each staker’s bond amount.

- **Reward Distribution:**  
  Rewards are distributed over time based on a schedule. The contract computes a global reward index and allocates rewards proportionally to stakers based on their bond amounts.

- **Reward Withdrawal:**  
  Stakers can withdraw their accumulated (pending) rewards via a dedicated execution function. Rewards can be sent as native tokens or via CW20 transfers.

- **CW20 Integration:**  
  The contract supports CW20 hook messages. This ensures that if the staking token is a CW20 token, only the designated token contract can trigger bonding.

- **Configurable Parameters:**  
  Upon instantiation, the contract is configured with:
  - `reward_token`: The token used for rewards.
  - `staking_token`: The token users stake.
  - `distribution_schedule`: A list of reward distribution slots defined as tuples `(start_time, end_time, amount)`.
  - `owner`: The contract owner (set during instantiation).

- **Administration:**  
  The owner can update the distribution schedule via `update_config` and can also trigger a migration of staking if needed.

## How It Works

1. **Instantiation:**  
   The contract is initialized with a configuration and state:
   - **Configuration:** Contains the owner, reward token, staking token, and distribution schedule.
   - **State:** Starts with no bonded tokens, a global reward index of zero, and records the last distribution timestamp.

2. **Bonding:**  
   - Users bond tokens either by sending native tokens or via a CW20 message (which is validated to ensure it comes from the proper staking token contract).
   - On bonding, the contract:
     - Updates the global reward index.
     - Computes and accumulates rewards for the staker.
     - Increases the staker’s bond amount.

3. **Unbonding:**  
   - Users may unbond a portion of their staked tokens.
   - The contract computes pending rewards before decreasing the bond amount.
   - If the staker’s bond and pending rewards drop to zero, their staking information is removed from storage.

4. **Reward Withdrawal:**  
   - Stakers can withdraw their pending rewards. The contract transfers rewards based on whether the reward token is native or a CW20 token.

5. **Reward Calculation:**  
   - Rewards are calculated using a distribution schedule and the passage of time.
   - The global reward index is updated based on the total bond amount and the amount distributed.
   - Each staker’s reward is computed as the difference between the product of their bond amount and the current global reward index, and their previously recorded reward index.

6. **Queries:**  
   The contract provides query endpoints to fetch:
   - Configuration details.
   - Current state (last distribution time, total bond amount, global reward index).
   - Individual staker information (bond amount, pending rewards, reward index).

## Deployment Steps

1. **Instantiate the Contract:**  
   Deploy the contract with the required parameters (owner, reward token, staking token, and distribution schedule).

2. **Bonding:**  
   Users can bond tokens to start earning rewards.

3. **Administration:**  
   The owner can update the distribution schedule using `update_config` and can migrate staking if required.

4. **Withdrawal:**  
   Stakers can unbond their tokens and withdraw rewards as needed.

