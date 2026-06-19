//! Schema generator: `cargo run -p choice_clmm_pool --example clmm_pool_schema`.
//! Emits JSON Schemas into `contracts/choice_clmm_pool/schema/`.
//! Message types live in the shared `choice_clmm_common` package.
use std::fs::create_dir_all;
use std::path::PathBuf;

use cosmwasm_schema::{export_schema, remove_schemas, schema_for};

use choice_clmm_common::pool::{
    DynamicFeeResponse, ExecuteMsg, FeeGrowthInsideResponse, InstantiateMsg, PositionInfoResponse,
    ProtocolFeesResponse, QuoteResponse, QueryMsg, TotalLiquidityResponse,
};

fn main() {
    let mut out_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    out_dir.push("schema");
    create_dir_all(&out_dir).unwrap();
    remove_schemas(&out_dir).unwrap();

    export_schema(&schema_for!(InstantiateMsg), &out_dir);
    export_schema(&schema_for!(ExecuteMsg), &out_dir);
    export_schema(&schema_for!(QueryMsg), &out_dir);
    export_schema(&schema_for!(ProtocolFeesResponse), &out_dir);
    export_schema(&schema_for!(PositionInfoResponse), &out_dir);
    export_schema(&schema_for!(TotalLiquidityResponse), &out_dir);
    export_schema(&schema_for!(QuoteResponse), &out_dir);
    export_schema(&schema_for!(FeeGrowthInsideResponse), &out_dir);
    export_schema(&schema_for!(DynamicFeeResponse), &out_dir);
}
