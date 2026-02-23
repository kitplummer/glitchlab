use anyhow::Result;

/// Execute the version command
pub async fn execute() -> Result<()> {
    println!("glitchlab 0.1.0");
    Ok(())
}
