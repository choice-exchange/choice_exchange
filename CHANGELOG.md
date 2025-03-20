# Change log

## v1.1.1

**Date:** `2025-03-20`  
**GitHub:** [@danvaneijck](https://github.com/danvaneijck)

- removed unused cw20 contract and accompanying code
- added check for lp funds in withdraw_liquidity function
- cleaned up all `cargo clippy` warnings
- all packages up to date
- fixed all `cargo audit` issues
- updated README docs for each contract
- added testnet deploy script
- freeze codebase for audit

## v1.1.0

**Date:** `2025-02-16`  
**GitHub:** [@danvaneijck](https://github.com/danvaneijck)

- added `choice_send_to_auction` for sending swap fees to burn auction
- forked anchor staking contract to `choice_farm` and added support for native staking and reward tokens
- adjusted `choice_factory` and `choice_pair` to use native denom as the liquidity token
