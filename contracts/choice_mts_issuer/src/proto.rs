//! Stargate-encoded proto messages this contract submits.
//!
//! All four types are hand-rolled with `prost` derives instead of pulled from
//! `injective-std`. The version of `injective-std` that ships these (≥1.19)
//! requires `cosmwasm-std = "3"` and conflicts with the rest of this
//! workspace (pinned at 2.2.x); the older `1.16.x` line that's compatible
//! also re-declares `cosmwasm-std` with `default-features = false`, which
//! breaks `cosmwasm-crypto`'s `ed25519-zebra` build under the v2 feature
//! resolver. Inlining keeps the dep graph clean.
//!
//! Field tags and type-urls must match the upstream protos exactly:
//!  * `injective.tokenfactory.v1beta1.MsgCreateDenom` — tag 6 (`bool
//!    allow_admin_burn`) is the v1.20+ chain field that enables the Path B
//!    admin-burn-from path; verified against
//!    `InjectiveFoundation/injective-core` and empirically by testnet probe
//!    2026-05-26 (see [`feedback_inj_tokenfactory_admin_burn`] memory).
//!  * `injective.tokenfactory.v1beta1.MsgBurn` — tag 3 (`burn_from_address`)
//!    is the field the issuer sets at `DeliverToSeeder` time.
//!  * `injective.erc20.v1beta1.MsgCreateTokenPair` — submitting with
//!    `token_pair.erc20_address = ""` asks the chain to auto-deploy a fresh
//!    `MintBurnBankERC20` whose owner is the lower-20-byte form of the
//!    `sender` bech32, making this contract both the tokenfactory admin and
//!    the EVM-side mint authority for the paired ERC20 in one shot.

use cosmwasm_std::{Binary, CosmosMsg};
use prost::Message;

/// Mirror of `cosmos.base.v1beta1.Coin`.
#[derive(Clone, PartialEq, Eq, Hash, prost::Message)]
pub struct ProtoCoin {
    #[prost(string, tag = "1")]
    pub denom: ::prost::alloc::string::String,
    /// Decimal string representation of the amount.
    #[prost(string, tag = "2")]
    pub amount: ::prost::alloc::string::String,
}

/// `injective.tokenfactory.v1beta1.MsgCreateDenom`.
#[derive(Clone, PartialEq, Eq, prost::Message)]
pub struct MsgCreateDenom {
    #[prost(string, tag = "1")]
    pub sender: ::prost::alloc::string::String,
    #[prost(string, tag = "2")]
    pub subdenom: ::prost::alloc::string::String,
    #[prost(string, tag = "3")]
    pub name: ::prost::alloc::string::String,
    #[prost(string, tag = "4")]
    pub symbol: ::prost::alloc::string::String,
    #[prost(uint32, tag = "5")]
    pub decimals: u32,
    /// Path B Leg A enabler — admins may `MsgBurn { burn_from_address: X }`
    /// against any holder when this is `true` and no permissions namespace
    /// is created for the denom.
    #[prost(bool, tag = "6")]
    pub allow_admin_burn: bool,
}

impl MsgCreateDenom {
    pub const TYPE_URL: &'static str = "/injective.tokenfactory.v1beta1.MsgCreateDenom";
}

/// `injective.tokenfactory.v1beta1.MsgBurn`. `burn_from_address` empty
/// defaults to self-burn; set it to a holder address to do an admin
/// burn-from on a denom that was created with `allow_admin_burn=true`.
#[derive(Clone, PartialEq, Eq, prost::Message)]
pub struct MsgBurn {
    #[prost(string, tag = "1")]
    pub sender: ::prost::alloc::string::String,
    #[prost(message, optional, tag = "2")]
    pub amount: ::core::option::Option<ProtoCoin>,
    #[prost(string, tag = "3")]
    pub burn_from_address: ::prost::alloc::string::String,
}

impl MsgBurn {
    pub const TYPE_URL: &'static str = "/injective.tokenfactory.v1beta1.MsgBurn";
}

/// `injective.erc20.v1beta1.TokenPair`.
#[derive(Clone, PartialEq, Eq, prost::Message)]
pub struct TokenPair {
    #[prost(string, tag = "1")]
    pub bank_denom: ::prost::alloc::string::String,
    #[prost(string, tag = "2")]
    pub erc20_address: ::prost::alloc::string::String,
}

/// `injective.erc20.v1beta1.MsgCreateTokenPair`.
#[derive(Clone, PartialEq, Eq, prost::Message)]
pub struct MsgCreateTokenPair {
    #[prost(string, tag = "1")]
    pub sender: ::prost::alloc::string::String,
    #[prost(message, optional, tag = "2")]
    pub token_pair: ::core::option::Option<TokenPair>,
}

impl MsgCreateTokenPair {
    pub const TYPE_URL: &'static str = "/injective.erc20.v1beta1.MsgCreateTokenPair";
}

/// Response proto for `MsgCreateTokenPair`. The SDK msg server returns this
/// in the SubMsg reply's `data` field; the reply handler decodes it to
/// capture the auto-deployed ERC20 address into
/// [`crate::state::LaunchRecord::erc20_address`].
#[derive(Clone, PartialEq, Eq, prost::Message)]
pub struct MsgCreateTokenPairResponse {
    #[prost(message, optional, tag = "1")]
    pub token_pair: ::core::option::Option<TokenPair>,
}

macro_rules! impl_stargate_msg {
    ($ty:ty) => {
        impl<T> From<$ty> for CosmosMsg<T> {
            fn from(msg: $ty) -> Self {
                let mut bytes = Vec::with_capacity(msg.encoded_len());
                // `EncodeError` only fires on insufficient capacity, which
                // can't happen when we pre-size with `encoded_len`.
                Message::encode(&msg, &mut bytes)
                    .expect(concat!(stringify!($ty), " encode infallible"));
                #[allow(deprecated)]
                CosmosMsg::Stargate {
                    type_url: <$ty>::TYPE_URL.to_string(),
                    value: Binary::new(bytes),
                }
            }
        }
    };
}

impl_stargate_msg!(MsgCreateDenom);
impl_stargate_msg!(MsgBurn);
impl_stargate_msg!(MsgCreateTokenPair);
