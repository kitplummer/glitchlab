#![allow(clippy::unwrap_used)]

use clap::CommandFactory;

use crate::Cli;

#[test]
fn verify_app() {
    Cli::command().debug_assert();
}

#[test]
fn test_long_version_format() {
    let long_version = Cli::command().render_long_version().to_string();
    let expected_format =
        regex::Regex::new(r"glitchlab \d+\.\d+\.\d+ \([0-9a-f]{7,40} \d{4}-\d{2}-\d{2}\)").unwrap();
    assert!(
        expected_format.is_match(&long_version),
        "Long version string format mismatch: {}",
        long_version
    );
}
