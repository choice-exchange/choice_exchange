
# Deployment and Configuration Guide

This guide explains how to deploy your contracts using the `deploy_testnet.sh` script and subsequently configure native token decimals for the Factory contract using the `add_native_token_decimals.sh` script. Follow the steps below to deploy and configure your contracts on the Injective network.

## Prerequisites

- **Injective CLI Installed:** Make sure the `injectived` CLI is installed and configured.
- **Network Access:** Confirm you have access to the appropriate node (testnet or mainnet).
- **Sufficient Funds:** Your deploying account must have enough funds to cover gas fees and transaction fees.
- **Bash Environment:** Both scripts are written for Bash; ensure you have a compatible shell.
- **Production contracts built:** Optimized production ready artifacts have been built using `build_release.sh`

## Files Overview

### `deploy_testnet.sh`

**Purpose:**  
This script is responsible for:
- **Storing Contracts:** Uploads the WASM files to the Injective blockchain.
- **Instantiating Contracts:** Deploys the following contracts:
  - **Pair Contract:** Manages the trading pair functionality.
  - **Factory Contract:** Central contract that creates pairs.
  - **Burn Manager Contract:** Handles sending token fees to the inj burn auction.
  - **CW20 Adapter Contract:** Bridges interactions with CW20 tokens.
  - **Router Contract:** Manages the routing logic for swaps.
- **Deployment Summary:** At the end, it prints out a summary with:
  - Code IDs for each contract.
  - Contract addresses (e.g., CW20 Adapter, Burn Manager, Factory, and Router).
  - Admin and Fee Wallet addresses.

**Key Variables in `deploy_testnet.sh`:**
- `NODE`: URL of the Injective node.
- `CHAIN_ID`: The chain ID of the network (e.g., `injective-888`).
- `FEES` & `GAS`: Fee and gas configurations for transactions.
- `FROM`: The account identifier used for sending transactions.
- `PASSWORD`: The password for the account.
- `ADMIN_ADDRESS` and `FEE_WALLET_ADDRESS`: Addresses used for administrative functions and fee collection.

### `add_native_token_decimals.sh`

**Purpose:**  
This script configures the Factory contract with the correct decimal precision for various native tokens. It:
- **Defines a List of Tokens:** An array (`TOKENS`) contains tokens in the format `denom:decimals`. You can add or remove tokens as needed.
- **Deposits Minimal Amounts:** For each token, a minimal bank send transaction is executed. This may be required for the factory to register the token.
- **Executes the Decimal-Adding Message:** Sends the `add_native_token_decimals` message to the Factory contract with the appropriate token denomination and its decimals.

**Key Variables in `add_native_token_decimals.sh`:**
- `NODE`, `CHAIN_ID`, `FEES`, `GAS`, `FROM`, `PASSWORD`: Similar to the deploy script.
- `FACTORY_CONTRACT`: **(Important!)** The address of your deployed Factory contract. You will update this after running `deploy_testnet.sh`.
- `ADMIN_ADDRESS`: The admin address used for sending the bank transactions.
- `TOKENS`: The list of native tokens and their corresponding decimals. Edit this list to suit your needs.

## Deployment Process

### Step 1: Run `deploy_testnet.sh`

1. **Open Your Terminal:**  
   Navigate to the directory containing the `deploy_testnet.sh` script.

2. **Check and Update Configuration:**  
   Ensure all configuration variables (like `NODE`, `CHAIN_ID`, `FEES`, etc.) are correctly set in the file.

3. **Execute the Script:**
   ```bash
   ./deploy_testnet.sh
   ```
   The script will:
   - Store and instantiate each contract.
   - Output a deployment summary with all relevant contract addresses and code IDs.
   
4. **Note the Output:**  
   From the summary, record the **Factory Contract Address** and the **Admin Address**. You will need these for the next step.

### Step 2: Configure and Run `add_native_token_decimals.sh`

1. **Edit the Script:**  
   Open `add_native_token_decimals.sh` in your text editor.  
   - **Update `FACTORY_CONTRACT`:** Replace the placeholder with the Factory contract address obtained from `deploy_testnet.sh`.
   - **Verify `ADMIN_ADDRESS`:** Ensure it matches the admin address from your deployment summary.
   - **Modify the Token List:** If needed, add or remove tokens from the `TOKENS` array. Each token should be in the format `"denom:decimals"` (e.g., `"inj:18"`).

2. **Run the Script:**
   ```bash
   ./add_native_token_decimals.sh
   ```
   The script will loop through each token in the list and:
   - Send a minimal bank transaction to the Factory contract.
   - Execute the `add_native_token_decimals` message to update the token’s decimal settings.

3. **Verify the Changes:**  
   Check the transaction logs to ensure that each token's decimals were successfully added to the Factory contract.

## Summary

- **Deploy Contracts:**  
  Run `deploy_testnet.sh` to deploy all necessary contracts. Note down the **Factory Contract Address** and **Admin Address** from the output.

- **Configure Token Decimals:**  
  Update `add_native_token_decimals.sh` with the Factory contract address and desired token configurations, then run the script.

By following these steps, you'll successfully deploy your contracts and configure the Factory contract with the correct native token decimal values.

Happy deploying!
