fn main() -> Result<(), Box<dyn std::error::Error>> {
    let descriptors = protox::compile(["proto/fleet.proto"], ["proto"])?;
    tonic_prost_build::configure()
        .build_transport(false)
        .compile_fds(descriptors)?;
    Ok(())
}
