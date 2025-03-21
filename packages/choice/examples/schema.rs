use std::env::current_dir;
use std::fs::create_dir_all;

use choice::asset::{Asset, AssetInfo, PairInfo};
use cosmwasm_schema::{export_schema, remove_schemas, schema_for};

fn main() {
    let mut out_dir = current_dir().unwrap();
    out_dir.push("schema");
    create_dir_all(&out_dir).unwrap();
    remove_schemas(&out_dir).unwrap();

    export_schema(&schema_for!(PairInfo), &out_dir);
    export_schema(&schema_for!(Asset), &out_dir);
    export_schema(&schema_for!(AssetInfo), &out_dir);
}
