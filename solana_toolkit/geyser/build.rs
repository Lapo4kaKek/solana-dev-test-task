fn main() -> Result<(), Box<dyn std::error::Error>> {
    tonic_build::configure()
        .build_server(false)
        .compile(&["src/proto/geyser.proto"], &["src/proto"])?;
    Ok(())
}
