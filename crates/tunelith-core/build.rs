fn main() {
    protobuf_codegen::Codegen::new()
        .pure()
        .include("proto")
        .input("proto/tunelith/v1/tunelith.proto")
        .cargo_out_dir("proto")
        .run_from_script();
}
