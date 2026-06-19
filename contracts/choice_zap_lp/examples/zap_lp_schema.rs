//! Schema generator: `cargo run -p choice-zap-lp --example zap_lp_schema`.
//! Emits JSON Schemas into `contracts/choice_zap_lp/schema/`.
use std::fs::create_dir_all;
use std::path::PathBuf;

use cosmwasm_schema::{export_schema, remove_schemas, schema_for};

use choice_zap_lp::msg::{
    ConfigResponse, ExecuteMsg, InstantiateMsg, IsKeeperResponse, KeepersResponse, MigrateMsg,
    QueryMsg, SimulateZapResponse,
};

fn main() {
    let mut out_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    out_dir.push("schema");
    create_dir_all(&out_dir).unwrap();
    remove_schemas(&out_dir).unwrap();

    export_schema(&schema_for!(InstantiateMsg), &out_dir);
    export_schema(&schema_for!(ExecuteMsg), &out_dir);
    export_schema(&schema_for!(QueryMsg), &out_dir);
    export_schema(&schema_for!(MigrateMsg), &out_dir);
    export_schema(&schema_for!(ConfigResponse), &out_dir);
    export_schema(&schema_for!(SimulateZapResponse), &out_dir);
    export_schema(&schema_for!(KeepersResponse), &out_dir);
    export_schema(&schema_for!(IsKeeperResponse), &out_dir);
}
