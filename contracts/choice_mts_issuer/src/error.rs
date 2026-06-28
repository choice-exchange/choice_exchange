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

    #[error("Issuer is paused; RegisterLaunch is halted")]
    Paused {},

    #[error("No pending admin to accept")]
    NoPendingAdmin {},

    #[error("instantiate does not accept funds (the CreateDenom fee is paid per-launch at RegisterLaunch)")]
    InstantiateDoesNotAcceptFunds {},

    #[error(
        "admin must not be the tokenfactory dead-burn address (would permanently brick governance)"
    )]
    AdminIsDeadAddress {},

    #[error("Subdenom prefix `{got}` is empty or exceeds the {max}-char cap")]
    SubdenomPrefixInvalid { got: String, max: usize },

    #[error("salt_suffix is required (Layer A anti-squat entropy): every launch must carry a high-entropy per-launch salt so its denom + sink + locker addresses are unguessable")]
    SaltSuffixRequired {},

    #[error(
        "salt_suffix `{got}` must be ASCII-alphanumeric and at least the minimum entropy length"
    )]
    SaltSuffixInvalid { got: String },

    #[error("forwarded CreateSink salt is not the canonical entropic sink salt (canonical(issuer) || be_u64(internal_id) || salt_suffix) — refusing to forward a non-entropic or mis-derived sink salt (anti-squat Layer A)")]
    SinkSaltMismatch {},

    #[error("clmm_pool_auth.ttl_seconds must be 0 (no-expiry reservation); got {got} — a lapsing reservation re-opens the CLMM pool-squat window (audit S-3)")]
    ClmmAuthTtlMustBeZero { got: u64 },

    #[error(
        "Constructed subdenom `{subdenom}` is {len} chars, over the {max}-char tokenfactory cap"
    )]
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

    #[error("Launch {id} status is {actual} but {expected} is required for this transition")]
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

    #[error("refund_deadline_seconds {got} is below the minimum {min}")]
    RefundDeadlineTooShort { got: u64, min: u64 },

    #[error("Migration state missing: no v1 config bytes at storage key `config`")]
    MigrationStateMissing {},

    #[error("Invalid migration: cannot apply variant `{requested}` from current version `{from}`")]
    InvalidMigration { from: String, requested: String },

    #[error("Seeder address `{addr}` holds no contract code — refusing to deliver cw_held to a ghost/EOA address")]
    SeederAddrNotAContract { addr: String },

    #[error("Seeder sink at `{addr}` is not a sink configured for this launch ({reason}) — refusing to deliver cw_held")]
    SeederSinkConfigMismatch { addr: String, reason: String },

    #[error("Launch {id} denom admin already renounced")]
    DenomAdminAlreadyRenounced { id: u64 },

    #[error("Reply id {id} is not handled by this contract")]
    UnknownReplyId { id: u64 },

    #[error("MsgCreateTokenPair reply for launch {id} did not include data")]
    CreateTokenPairReplyMissingData { id: u64 },

    #[error("MsgCreateTokenPair reply could not be decoded: {0}")]
    CreateTokenPairReplyDecode(String),

    #[error("Cannot migrate: stored contract is `{found}`, expected `{expected}`")]
    MigrationWrongContract { found: String, expected: String },

    #[error("create_sink_payload is not a `CreateSink` message: {0}")]
    SinkPayloadDecode(String),

    #[error("create_sink_payload {field} `{got}` does not match RegisterLaunch `{expected}`")]
    SinkPayloadMismatch {
        field: String,
        got: String,
        expected: String,
    },

    #[error("CLMM launch requires `clmm_pool_auth` to reserve the pool slot (anti-squat)")]
    ClmmAuthRequired {},

    #[error("clmm_pool_auth.fee {auth_fee} does not match the sink's CLMM fee_tier {sink_fee}")]
    ClmmAuthFeeMismatch { auth_fee: u32, sink_fee: u32 },

    #[error("clmm_pool_auth.clmm_factory `{auth_factory}` does not match the sink's clmm_factory `{sink_factory}`")]
    ClmmAuthFactoryMismatch {
        auth_factory: String,
        sink_factory: String,
    },

    #[error("clmm_pool_auth was supplied for a non-CLMM (XYK) launch")]
    ClmmAuthUnexpected {},

    #[error("pair_denom must differ from the launch denom `{denom}` (no self-paired pool)")]
    PairDenomEqualsLaunchDenom { denom: String },

    #[error(
        "could not query the seeder factory `{addr}` FactoryConfig for sink_code_id: {reason}"
    )]
    SeederFactoryConfigQuery { addr: String, reason: String },

    #[error("could not query CodeInfo (checksum) for sink_code_id {code_id}: {reason}")]
    SinkCodeInfoQuery { code_id: u64, reason: String },

    #[error("instantiate2 address derivation failed: {0}")]
    SeederAddrDerivation(String),

    #[error(
        "seeder_addr `{supplied}` is not the Instantiate2 sink for this launch (sink_code_id {sink_code_id} derives `{derived}`) — refusing to register a look-alike sink"
    )]
    SeederAddrDerivationMismatch {
        supplied: String,
        derived: String,
        sink_code_id: u64,
    },

    #[error(
        "sink refund_receiver `{supplied}` must equal the launch's evm_authority `{expected}` — a CLMM launch's failure-path pair-asset is only allowed to route back to the LaunchpadCore that does the proportional refund (audit H-1)"
    )]
    RefundReceiverMismatch { supplied: String, expected: String },

    #[error(
        "CLMM position_recipient `{supplied}` is not the Instantiate2 locker for this launch (derives `{derived}`) — refusing to register a launch whose graduation position NFT would mint anywhere but the no-withdraw locker (audit H-1)"
    )]
    LockerAddrDerivationMismatch { supplied: String, derived: String },

    #[error(
        "refusing to deliver cw_held: this launch was registered while seeder-address derivation was DISABLED (seeder_verified=false) and verification is now required — flip SetVerifySeederDerivation off to use the escape hatch, or RefundFailedLaunch instead (audit M-3)"
    )]
    SeederDerivationNotVerified {},

    #[error("Issuer is paused; keeper-initiated RefundFailedLaunch is halted (the admin may still refund past the deadline)")]
    KeeperRefundPaused {},
}
