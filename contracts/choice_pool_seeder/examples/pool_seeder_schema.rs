//! Schema generator: `cargo run -p choice-pool-seeder --example pool_seeder_schema`.
//!
//! Emits one JSON Schema per wire type into `contracts/choice_pool_seeder/schema/`.
//! These are the machine-readable API surface for external consumers — feed the
//! directory to `@cosmwasm/ts-codegen` (or any JSON-Schema client generator) to
//! produce typed clients. Regenerate and commit whenever `msg.rs` changes.
//!
//! Kept as an `examples/` target (the choice_exchange convention) so the wasm
//! workspace-optimizer never tries to compile this host-only binary.
use std::fs::create_dir_all;
use std::path::PathBuf;

use cosmwasm_schema::{export_schema, remove_schemas, schema_for};

use choice_pool_seeder::msg::{
    CallbackMsg, ExecuteMsg, FactoryConfigResponse, InstantiateMsg, LockerConfigResponse,
    MigrateMsg, QueryMsg, RoleResponse, SinkConfigResponse, SinkStateResponse,
};

fn main() {
    // Write to the crate's own `schema/` dir regardless of the invocation cwd.
    let mut out_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    out_dir.push("schema");
    create_dir_all(&out_dir).unwrap();
    remove_schemas(&out_dir).unwrap();

    export_schema(&schema_for!(InstantiateMsg), &out_dir);
    export_schema(&schema_for!(ExecuteMsg), &out_dir);
    export_schema(&schema_for!(CallbackMsg), &out_dir);
    export_schema(&schema_for!(QueryMsg), &out_dir);
    export_schema(&schema_for!(MigrateMsg), &out_dir);
    export_schema(&schema_for!(RoleResponse), &out_dir);
    export_schema(&schema_for!(FactoryConfigResponse), &out_dir);
    export_schema(&schema_for!(SinkConfigResponse), &out_dir);
    export_schema(&schema_for!(SinkStateResponse), &out_dir);
    export_schema(&schema_for!(LockerConfigResponse), &out_dir);
}
