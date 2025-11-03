use std::env::current_dir;
use std::fs::create_dir_all;

use cosmwasm_schema::{export_schema, remove_schemas, schema_for};

// Import all the messages, responses, and state structs from your vault contract
use choice_vault::msg::{
    Cw20HookMsg, ExecuteMsg, InstantiateMsg, PendingDepositsResponse, QueryMsg, UserInfoResponse,
};
use choice_vault::state::Config;

fn main() {
    // Get the project's root directory
    let mut out_dir = current_dir().unwrap();
    // Set the output directory to `./schema`
    out_dir.push("schema");

    // Create the directory if it doesn't exist
    create_dir_all(&out_dir).unwrap();
    // Clean the directory to ensure old schema files are removed
    remove_schemas(&out_dir).unwrap();

    // Export schemas for the main messages
    export_schema(&schema_for!(InstantiateMsg), &out_dir);
    export_schema(&schema_for!(ExecuteMsg), &out_dir);
    export_schema(&schema_for!(QueryMsg), &out_dir);

    // Export the schema for the Cw20 hook message (good for integration)
    export_schema(&schema_for!(Cw20HookMsg), &out_dir);

    // Export schemas for important state and query response structs
    export_schema(&schema_for!(Config), &out_dir);
    export_schema(&schema_for!(UserInfoResponse), &out_dir);
    export_schema(&schema_for!(PendingDepositsResponse), &out_dir);

    println!("JSON schemas generated to the directory: ./schema");
}
