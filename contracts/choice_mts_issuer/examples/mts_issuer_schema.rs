//! Schema generator: `cargo run -p choice-mts-issuer --example mts_issuer_schema`.
//!
//! Emits one JSON Schema per wire type into `contracts/choice_mts_issuer/schema/`.
//! These are the machine-readable API surface for external consumers — feed the
//! directory to `@cosmwasm/ts-codegen` (or any JSON-Schema client generator) to
//! produce typed clients. Regenerate and commit whenever `msg.rs` changes.
//!
//! Kept as an `examples/` target (the choice_exchange convention) so the wasm
//! workspace-optimizer never tries to compile this host-only binary.
use std::fs::create_dir_all;
use std::path::PathBuf;

use cosmwasm_schema::{export_schema, remove_schemas, schema_for};

use choice_mts_issuer::msg::{
    ConfigResponse, ExecuteMsg, InstantiateMsg, LaunchResponse, LaunchesResponse, MigrateMsg,
    QueryMsg,
};

fn main() {
    // Write to the crate's own `schema/` dir regardless of the invocation cwd.
    let mut out_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    out_dir.push("schema");
    create_dir_all(&out_dir).unwrap();
    remove_schemas(&out_dir).unwrap();

    export_schema(&schema_for!(InstantiateMsg), &out_dir);
    export_schema(&schema_for!(ExecuteMsg), &out_dir);
    export_schema(&schema_for!(QueryMsg), &out_dir);
    export_schema(&schema_for!(MigrateMsg), &out_dir);
    export_schema(&schema_for!(ConfigResponse), &out_dir);
    export_schema(&schema_for!(LaunchResponse), &out_dir);
    export_schema(&schema_for!(LaunchesResponse), &out_dir);
}
