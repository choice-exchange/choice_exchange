# Audit fixes

## 1. Fix for Audit Finding: Funds Sent for Verifying Native Token Decimals Can Be Stolen

## Summary

A `MODERATE` risk finding was identified where native tokens sent to the `choice_factory` contract (for verifying token decimals via `execute_add_native_token_decimals`) could become permanently stuck or be exploited in combination with liquidity creation operations.  
To address this, we have implemented a secure withdrawal mechanism, restricted to the contract owner, allowing recovery of any accidentally sent native tokens.

---

## Changes Implemented

A new execution message `WithdrawNative` was added to the `ChoiceFactoryExecuteMsg` enum:

```rust
pub enum ChoiceFactoryExecuteMsg {
    // ... existing variants
    WithdrawNative {
        denom: String,
        amount: Uint128,
    },
}
```

The corresponding handler `execute_withdraw_native` was added:

```rust
pub fn execute_withdraw_native(
    deps: DepsMut<InjectiveQueryWrapper>,
    _env: Env,
    info: MessageInfo,
    denom: String,
    amount: Uint128,
) -> Result<Response, StdError> {
    // Only owner can withdraw
    let config = CONFIG.load(deps.storage)?;
    if deps.api.addr_canonicalize(info.sender.as_str())? != config.owner {
        return Err(StdError::generic_err("Unauthorized"));
    }

    // Send the specified amount to the owner
    let bank_msg = BankMsg::Send {
        to_address: info.sender.to_string(),
        amount: vec![Coin {
            denom,
            amount,
        }],
    };

    Ok(Response::new()
        .add_message(bank_msg)
        .add_attribute("action", "withdraw_native")
        .add_attribute("owner", info.sender)
    )
}
```

The new message is handled in the `execute` router:

```rust
ExecuteMsg::WithdrawNative { denom, amount } => {
    execute_withdraw_native(deps, env, info, denom, amount)
}
```

---

## Security Considerations

- **Access Control**: The withdrawal function is protected by an ownership check. Only the address configured as `config.owner` can invoke it.
- **Scope**: This mechanism only enables withdrawal of native tokens accidentally sent to the factory contract itself. It **does not** affect liquidity locked in `choice_pair` contracts.
- **Impact**: There is no impact to users or liquidity providers. The fix ensures the factory contract cannot unintentionally hold or lose funds.

---

## 2. Incorrect refund asset event emitted for CW20 tokens

**Summary:**  
In `provide_liquidity`, a `remain_amount` was incorrectly calculated and emitted for CW20 tokens, even though excess sending is impossible.  
We updated the logic to set `remain_amount = 0` for CW20 assets, ensuring that refund events are only emitted when necessary.

**Fix:**  
Added a check to override `remain_amount` to zero for CW20 tokens before pushing to `refund_assets`.

---