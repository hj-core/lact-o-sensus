fn main() -> Result<(), Box<dyn std::error::Error>> {
    tonic_prost_build::configure()
        .extern_path(".google.protobuf.Timestamp", "::prost_types::Timestamp")
        .extern_path(
            ".lacto_sensus.v1.GroceryItem",
            "::common::proto::v1::app::GroceryItem",
        )
        .extern_path(
            ".lacto_sensus.v1.SessionRecord",
            "::common::proto::v1::app::SessionRecord",
        )
        .compile_protos(&["proto/internal.proto"], &["proto", "../common/proto"])?;
    Ok(())
}
