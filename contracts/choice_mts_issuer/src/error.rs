use cosmwasm_std::{ConversionOverflowError, OverflowError, StdError};
use thiserror::Error;

#[derive(Error, Debug, PartialEq)]
pub enum ContractError {
    #[error("{0}")]
    Std(#[from] StdError),

    #[error("{0}")]
    OverflowError(#[from] OverflowError),

    #[error("{0}")]
    ConversionOverflowError(#[from] ConversionOverflowError),

    #[error("Unauthorized")]
    Unauthorized {},

    #[error("Caller is not the configured keeper")]
    NotKeeper {},

    #[error("Subdenom prefix `{got}` is empty or exceeds the {max}-char cap")]
    SubdenomPrefixInvalid { got: String, max: usize },

    #[error("salt_suffix `{got}` must be non-empty and ASCII-alphanumeric")]
    SaltSuffixInvalid { got: String },

    #[error("Constructed subdenom `{subdenom}` is {len} chars, over the {max}-char tokenfactory cap")]
    SubdenomTooLong {
        subdenom: String,
        len: usize,
        max: usize,
    },

    #[error("Decimals {got} out of range (must be 0..=18)")]
    DecimalsOutOfRange { got: u32 },

    #[error("RegisterLaunch already processed internal_id {id}")]
    LaunchAlreadyRegistered { id: u64 },

    #[error("Launch {id} not found")]
    LaunchNotFound { id: u64 },

    #[error(
        "Launch {id} status is {actual} but {expected} is required for this transition"
    )]
    InvalidLaunchStatus {
        id: u64,
        actual: String,
        expected: String,
    },

    #[error("total_supply must be > 0")]
    ZeroTotalSupply {},

    #[error("evm_supply {evm_supply} exceeds total_supply {total_supply}")]
    EvmSupplyExceedsTotal {
        evm_supply: String,
        total_supply: String,
    },

    #[error(
        "leftover {leftover} exceeds evm_supply {evm_supply} — can't burn more than what the EVM authority received"
    )]
    LeftoverExceedsEvmSupply {
        leftover: String,
        evm_supply: String,
    },

    #[error(
        "RegisterLaunch with choice_factory requires cw_held >= 1 (1 wei dust funds AddNativeTokenDecimals): total_supply={total_supply}, evm_supply={evm_supply}"
    )]
    ChoiceFactoryNeedsDust {
        total_supply: String,
        evm_supply: String,
    },

    #[error(
        "Attach the tokenfactory create fee in `info.funds`: need {required} {denom}, supplied {supplied}"
    )]
    InsufficientCreateFee {
        denom: String,
        required: String,
        supplied: String,
    },

    #[error(
        "info.funds must equal the chain's create fee exactly: {denom} required {required}, supplied {supplied}"
    )]
    CreateFeeOverpaid {
        denom: String,
        required: String,
        supplied: String,
    },

    #[error("info.funds carries unexpected denom `{denom}` (only the chain's create fee may be attached)")]
    UnexpectedFundsDenom { denom: String },

    #[error("Refund deadline not yet reached: {remaining_seconds}s remaining")]
    RefundDeadlineNotReached { remaining_seconds: u64 },

    #[error("Migration state missing: no v1 config bytes at storage key `config`")]
    MigrationStateMissing {},

    #[error(
        "Invalid migration: cannot apply variant `{requested}` from current version `{from}`"
    )]
    InvalidMigration { from: String, requested: String },

    #[error("Seeder address `{addr}` holds no contract code — refusing to deliver cw_held to a ghost/EOA address")]
    SeederAddrNotAContract { addr: String },

    #[error("Launch {id} denom admin already renounced")]
    DenomAdminAlreadyRenounced { id: u64 },

    #[error("Reply id {id} is not handled by this contract")]
    UnknownReplyId { id: u64 },

    #[error("MsgCreateTokenPair reply for launch {id} did not include data")]
    CreateTokenPairReplyMissingData { id: u64 },

    #[error("MsgCreateTokenPair reply could not be decoded: {0}")]
    CreateTokenPairReplyDecode(String),
}
