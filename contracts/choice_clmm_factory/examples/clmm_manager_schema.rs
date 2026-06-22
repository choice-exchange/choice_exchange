//! Schema generator for the **choice_clmm_manager** contract:
//! `cargo run -p choice_clmm_factory --example clmm_manager_schema`.
//!
//! Why it lives in the *factory* crate rather than the manager crate: the
//! manager depends on `cw721-base`, which pulls `cosmwasm-crypto` and fails to
//! compile on the host target (`ed25519-zebra` loses its `batch` feature off
//! wasm32), so a manager-local `examples/` target can't build. The manager's
//! message types live in the shared `choice_clmm_common` package, and the
//! factory crate IS host-buildable and depends on the same package — so we
//! generate the manager's schema from here and write it into the manager's own
//! `schema/` dir.
use std::fs::create_dir_all;
use std::path::PathBuf;

use cosmwasm_schema::{export_schema, remove_schemas, schema_for};

use choice_clmm_common::manager::{ExecuteMsg, InstantiateMsg, PositionWithFeesResponse, QueryMsg};

fn main() {
    // Sibling contract's schema dir (this example runs from the factory crate).
    let mut out_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    out_dir.push("../choice_clmm_manager/schema");
    create_dir_all(&out_dir).unwrap();
    remove_schemas(&out_dir).unwrap();

    export_schema(&schema_for!(InstantiateMsg), &out_dir);
    export_schema(&schema_for!(ExecuteMsg), &out_dir);
    export_schema(&schema_for!(QueryMsg), &out_dir);
    export_schema(&schema_for!(PositionWithFeesResponse), &out_dir);
}
