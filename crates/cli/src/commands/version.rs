use anyhow::Result;

/// Execute the version command
pub async fn execute() -> Result<()> {
    println!("glitchlab {}", env!("CARGO_PKG_VERSION"));
    Ok(())
}
