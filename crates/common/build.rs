fn main() -> Result<(), Box<dyn std::error::Error>> {
    tonic_prost_build::configure()
        .extern_path(".google.protobuf.Timestamp", "::prost_types::Timestamp")
        .compile_protos(&["proto/lacto_sensus.proto"], &["proto"])?;
    Ok(())
}
