// Compiles the vendored SpiceDB protos with protox (a protobuf compiler in Rust), so building needs no protoc.
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let files = [
        "proto/authzed/api/v1/permission_service.proto",
        "proto/authzed/api/v1/schema_service.proto",
        "proto/authzed/api/v1/watch_service.proto",
    ];
    for f in files {
        println!("cargo:rerun-if-changed={f}");
    }
    let fds = protox::compile(files, ["proto"])?;
    tonic_prost_build::configure()
        .build_server(false)
        .compile_fds(fds)?;
    Ok(())
}
