fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Compile ward.proto into Rust types and gRPC service traits.
    tonic_build::configure()
        .build_server(true)
        .build_client(true)
        .compile_protos(&["../proto/ward.proto"], &["../proto"])?;

    // Link libkrun only when the krunvm feature is enabled.
    // Without this feature, the backend module compiles with stub
    // implementations that return errors.
    #[cfg(feature = "krunvm")]
    {
        println!("cargo:rustc-link-lib=krun");
        println!("cargo:rustc-link-lib=krunfw");
    }

    Ok(())
}
