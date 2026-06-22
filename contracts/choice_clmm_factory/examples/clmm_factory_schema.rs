//! Schema generator: `cargo run -p choice_clmm_factory --example clmm_factory_schema`.
//! Emits JSON Schemas into `contracts/choice_clmm_factory/schema/`.
//! Message types live in the shared `choice_clmm_common` package.
use std::fs::create_dir_all;
use std::path::PathBuf;

use cosmwasm_schema::{export_schema, remove_schemas, schema_for};

use choice_clmm_common::factory::{
    ConfigResponse, CreationAuthResponse, ExecuteMsg, FlashBorrowersResponse, InstantiateMsg,
    IsFlashBorrowerResponse, QueryMsg,
};

fn main() {
    let mut out_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    out_dir.push("schema");
    create_dir_all(&out_dir).unwrap();
    remove_schemas(&out_dir).unwrap();

    export_schema(&schema_for!(InstantiateMsg), &out_dir);
    export_schema(&schema_for!(ExecuteMsg), &out_dir);
    export_schema(&schema_for!(QueryMsg), &out_dir);
    export_schema(&schema_for!(ConfigResponse), &out_dir);
    export_schema(&schema_for!(CreationAuthResponse), &out_dir);
    export_schema(&schema_for!(IsFlashBorrowerResponse), &out_dir);
    export_schema(&schema_for!(FlashBorrowersResponse), &out_dir);
}
