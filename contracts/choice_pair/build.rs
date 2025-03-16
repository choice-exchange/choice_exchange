extern crate protobuf_codegen;
extern crate protoc_bin_vendored;

fn main() {
    // Re-run this build script if the proto file changes.
    println!("cargo:rerun-if-changed=src/response.proto");

    protobuf_codegen::Codegen::new()
        .protoc()
        .protoc_path(&protoc_bin_vendored::protoc_bin_path().unwrap())
        // The .proto file is in the src folder.
        .includes(["src"])
        .input("src/response.proto")
        // Output the generated code directly into src.
        .out_dir("src")
        .run_from_script();
}
