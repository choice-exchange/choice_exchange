# Audit fixes

## 1. Funds Sent for Verifying Native Token Decimals Can Be Stolen 

**Issue Summary:**  
A MODERATE risk was identified where native tokens sent to the `choice_factory` contract (to verify token decimals) could become permanently stuck or exploited during liquidity creation.

**Fix:**  
We introduced a secure withdrawal mechanism that allows only the contract owner to recover accidentally sent native tokens. A new `WithdrawNative` execution message was added, and ownership checks were enforced to ensure only the authorized owner can perform withdrawals.

**Security Considerations:**
- **Access control:** Only the owner can withdraw native tokens, preventing unauthorized access.
- **Limited scope:** Withdrawals are limited to native tokens sent directly to the factory contract; liquidity and user funds in pairs are unaffected.
- **Impact:** No changes to liquidity pool behavior. This fix ensures that tokens mistakenly sent to the factory are safely recoverable and the contract cannot accumulate unclaimed funds.

---

## 2. Incorrect refund asset event emitted for CW20 tokens

**Summary:**  
In `provide_liquidity`, a `remain_amount` was incorrectly calculated and emitted for CW20 tokens, even though excess sending is impossible.  
We updated the logic to set `remain_amount = 0` for CW20 assets, ensuring that refund events are only emitted when necessary.

**Fix:**  
Added a check to override `remain_amount` to zero for CW20 tokens before pushing to `refund_assets`.

---

## 3. Addresses and subaccounts are not validated

**Issue Summary:**  
Certain fields in the `choice_send_to_auction` contract (`msg.admin`, `msg.adapter_contract`, and `msg.burn_auction_subaccount`) were not properly validated during instantiation and configuration updates. This could lead to invalid addresses or subaccount values being stored.

**Fix:**  
We added the following validations:
- Used `deps.api.addr_validate` to validate `msg.admin` and `msg.adapter_contract` during contract instantiation.
- Validated `msg.admin` again during configuration updates in `execute_update_config`.
- Used `SubaccountId::new(...)` with error handling to validate the `msg.burn_auction_subaccount` string format during instantiation.

These changes ensure that only properly formatted addresses and subaccount IDs can be stored, improving contract safety and aligning with audit recommendations.

## 4. LP Amount Calculation Fix

**Issue Summary:**  
In the `choice_pair` contract, the `lp_amount` value (representing liquidity provider rewards) was previously calculated as `2/3` of the total fee. Due to integer rounding, this caused minor inconsistencies between the actual distributed amounts and the emitted `pool_amount` event.

**Fix:**  
We updated the `lp_amount` calculation to use the formula:  
`lp_amount = total_fee - fee_wallet_amount - burn_amount`.  
This ensures that `lp_amount` always accurately reflects the remaining amount after subtracting the exact `fee_wallet_amount` and `burn_amount`, avoiding rounding errors and making the emitted `pool_amount` event correct.

---

## 5. Two-Step Ownership Transfer Implementation

**Issue Summary:**  
Both the `choice_factory` and `choice_send_to_auction` contracts originally allowed ownership to be transferred immediately without confirmation from the new owner. This carried a risk: if the owner mistakenly transferred ownership to an invalid address or an address they did not control, ownership would be permanently lost.

**Fix:**  
We implemented a **two-step ownership transfer** mechanism in both contracts:
- The current owner must first call a `ProposeNewOwner` function, specifying the new intended owner.
- The proposed new owner must then call `AcceptOwnership` to finalize the transfer.
- Until the proposed owner accepts, the original owner remains fully in control and can cancel the pending proposal if needed (optional future addition).

This approach ensures that ownership can only be transferred intentionally and safely, protecting the contract from accidental or malicious transfers to incorrect addresses.  
The `UpdateConfig` function was also updated to remove direct owner setting to fully enforce the two-step transfer process.

---

