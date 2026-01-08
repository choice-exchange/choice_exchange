use cosmwasm_schema::{cw_serde, QueryResponses};
use cosmwasm_std::{CustomMsg, Uint128};
use cw721::{msg::NftExtensionMsg, state::Trait};

// The data stored on the NFT
#[cw_serde]
pub struct Position {
    pub token0: String,
    pub token1: String,
    pub fee: u32,
    pub tick_lower: i32,
    pub tick_upper: i32,
    // The address of the actual pool contract
    pub pool_address: String,
}

impl CustomMsg for Position {}

impl From<Position> for Vec<Trait> {
    fn from(p: Position) -> Self {
        vec![
            Trait {
                display_type: None,
                trait_type: "Token 0".to_string(),
                value: p.token0,
            },
            Trait {
                display_type: None,
                trait_type: "Token 1".to_string(),
                value: p.token1,
            },
            Trait {
                display_type: None,
                trait_type: "Fee Tier".to_string(),
                value: p.fee.to_string(),
            },
            Trait {
                display_type: Some("number".to_string()),
                trait_type: "Lower Tick".to_string(),
                value: p.tick_lower.to_string(),
            },
            Trait {
                display_type: Some("number".to_string()),
                trait_type: "Upper Tick".to_string(),
                value: p.tick_upper.to_string(),
            },
            Trait {
                display_type: None,
                trait_type: "Pool Address".to_string(),
                value: p.pool_address,
            },
        ]
    }
}

// 4. Implement conversion to Standard Metadata (NftExtensionMsg)
// This is what marketplaces actually read when they query `NftInfo`.
impl From<Position> for NftExtensionMsg {
    fn from(p: Position) -> Self {
        // You can even generate an SVG here like Uniswap V3 does!
        // For now, we just output the attributes.
        NftExtensionMsg {
            name: Some(format!("CLMM Position: {}/{}", p.token0, p.token1)),
            description: Some(format!(
                "Liquidity Position for {}/{} (Fee: {}). Range: [{}, {}]",
                p.token0, p.token1, p.fee, p.tick_lower, p.tick_upper
            )),
            image: None,                // TODO: Generate SVG
            attributes: Some(p.into()), // Uses the vec![] impl above

            // Standard defaults
            animation_url: None,
            background_color: None,
            external_url: None,
            image_data: None,
            youtube_url: None,
        }
    }
}

#[cw_serde]
pub struct InstantiateMsg {
    pub name: String,
    pub symbol: String,
    pub factory_addr: String,
}

#[cw_serde]
pub enum ExecuteMsg {
    // --- CLMM Specific Logic ---
    /// Create a new Position (Mint NFT + Add Liquidity)
    /// Funds for the pool must be sent with this message
    MintPosition {
        token0: String,
        token1: String,
        fee: u32,
        tick_lower: i32,
        tick_upper: i32,
        amount0_desired: Uint128,
        amount1_desired: Uint128,
        amount0_min: Uint128,
        amount1_min: Uint128,
        recipient: Option<String>,
        deadline: u64,
    },

    /// Add more liquidity to an existing NFT
    IncreaseLiquidity {
        token_id: String,
        amount0_desired: Uint128,
        amount1_desired: Uint128,
        amount0_min: Uint128,
        amount1_min: Uint128,
        deadline: u64,
    },

    /// Remove liquidity (Principal) from the NFT
    DecreaseLiquidity {
        token_id: String,
        liquidity: Uint128,
        amount0_min: Uint128,
        amount1_min: Uint128,
        deadline: u64,
    },

    /// Collect unclaimed fees
    Collect {
        token_id: String,
        recipient: Option<String>, // Defaults to owner
    },

    /// Burn the NFT (only if liquidity is 0 and fees collected)
    Burn {
        token_id: String,
    },

    // --- Standard CW721 Logic (Delegated) ---
    // We allow transfers, but we block internal Mint/Burn calls
    // because they must go through the logic above.
    TransferNft {
        recipient: String,
        token_id: String,
    },
    SendNft {
        contract: String,
        token_id: String,
        msg: cosmwasm_std::Binary,
    },
    Approve {
        spender: String,
        token_id: String,
        expires: Option<cw_utils::Expiration>,
    },
    Revoke {
        spender: String,
        token_id: String,
    },
    ApproveAll {
        operator: String,
        expires: Option<cw_utils::Expiration>,
    },
    RevokeAll {
        operator: String,
    },
}

#[cw_serde]
#[derive(QueryResponses)]
pub enum QueryMsg {
    // Custom: Get position details
    #[returns(Position)]
    Position { token_id: String },

    // Standard CW721
    #[returns(cw721::msg::OwnerOfResponse)]
    OwnerOf {
        token_id: String,
        include_expired: Option<bool>,
    },
    #[returns(cw721::msg::ApprovalResponse)]
    Approval {
        token_id: String,
        spender: String,
        include_expired: Option<bool>,
    },
    #[returns(cw721::msg::ApprovalsResponse)]
    Approvals {
        token_id: String,
        include_expired: Option<bool>,
    },
    #[returns(cw721::msg::NumTokensResponse)]
    NumTokens {},
    #[returns(cw721::msg::NftInfoResponse<Position>)]
    NftInfo { token_id: String },
    #[returns(cw721::msg::AllNftInfoResponse<Position>)]
    AllNftInfo {
        token_id: String,
        include_expired: Option<bool>,
    },
    #[returns(cw721::msg::TokensResponse)]
    Tokens {
        owner: String,
        start_after: Option<String>,
        limit: Option<u32>,
    },
}
