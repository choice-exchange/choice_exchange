use crate::state::TICKS;
use cosmwasm_std::{StdResult, Storage, Uint256};

pub fn get_fee_growth_inside(
    storage: &dyn Storage,
    lower_tick: i32,
    upper_tick: i32,
    current_tick: i32,
    fee_growth_global_0: Uint256,
    fee_growth_global_1: Uint256,
) -> StdResult<(Uint256, Uint256)> {
    let lower = TICKS.load(storage, lower_tick)?;
    let upper = TICKS.load(storage, upper_tick)?;

    // Calculate fee growth below lower tick
    let fee_growth_below_0 = if current_tick >= lower_tick {
        lower.fee_growth_outside_0
    } else {
        fee_growth_global_0.wrapping_sub(lower.fee_growth_outside_0)
    };
    let fee_growth_below_1 = if current_tick >= lower_tick {
        lower.fee_growth_outside_1
    } else {
        fee_growth_global_1.wrapping_sub(lower.fee_growth_outside_1)
    };

    // Calculate fee growth above upper tick
    let fee_growth_above_0 = if current_tick < upper_tick {
        upper.fee_growth_outside_0
    } else {
        fee_growth_global_0.wrapping_sub(upper.fee_growth_outside_0)
    };
    let fee_growth_above_1 = if current_tick < upper_tick {
        upper.fee_growth_outside_1
    } else {
        fee_growth_global_1.wrapping_sub(upper.fee_growth_outside_1)
    };

    let fee_growth_inside_0 = fee_growth_global_0
        .wrapping_sub(fee_growth_below_0)
        .wrapping_sub(fee_growth_above_0);
    let fee_growth_inside_1 = fee_growth_global_1
        .wrapping_sub(fee_growth_below_1)
        .wrapping_sub(fee_growth_above_1);

    Ok((fee_growth_inside_0, fee_growth_inside_1))
}
