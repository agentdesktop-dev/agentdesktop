fn main() -> Result<(), Box<dyn std::error::Error>> {
    tonic_prost_build::configure()
        .build_transport(false)
        .compile_protos(&["proto/fleet.proto"], &["proto"])?;
    Ok(())
}
