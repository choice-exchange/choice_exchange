//! Schema generator: `cargo run -p choice-farm-factory --example farm_factory_schema`.
//! Emits JSON Schemas into `contracts/choice_farm_factory/schema/`.
//! Message types live in the shared `choice` package (`choice::farm_factory`).
use std::fs::create_dir_all;
use std::path::PathBuf;

use cosmwasm_schema::{export_schema, remove_schemas, schema_for};

use choice::farm_factory::{
    ConfigResponse, ExecuteMsg, FarmCountResponse, FarmsResponse, InstantiateMsg, MigrateMsg,
    PendingFarmCodeIdUpdateResponse, PendingOwnerRotationResponse, QueryMsg,
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
    export_schema(&schema_for!(PendingOwnerRotationResponse), &out_dir);
    export_schema(&schema_for!(PendingFarmCodeIdUpdateResponse), &out_dir);
    export_schema(&schema_for!(FarmsResponse), &out_dir);
    export_schema(&schema_for!(FarmCountResponse), &out_dir);
}
