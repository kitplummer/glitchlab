use anyhow::Result;

/// Execute the version command.
/// Prints the crate version in the format "glitchlab X.Y.Z".
pub async fn execute() -> Result<()> {
    println!("glitchlab {}", env!("CARGO_PKG_VERSION"));
    Ok(())
}
