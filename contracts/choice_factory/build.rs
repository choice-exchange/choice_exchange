extern crate protobuf_codegen;
extern crate protoc_bin_vendored;

fn main() {
    // Re-run this build script if the proto file changes.
    println!("cargo:rerun-if-changed=src/response.proto");

    protobuf_codegen::Codegen::new()
        // Use the bundled protoc binary.
        .protoc()
        .protoc_path(&protoc_bin_vendored::protoc_bin_path().unwrap())
        // Tell protoc where to look for imports; here, the .proto is in src.
        .includes(["src"])
        // Specify the input .proto file.
        .input("src/response.proto")
        // Output the generated Rust code into the same directory ("src").
        .out_dir("src")
        // Execute the code generation.
        .run_from_script();
}
