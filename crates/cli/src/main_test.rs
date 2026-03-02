#![allow(clippy::unwrap_used)]

use clap::{CommandFactory, Parser};

use crate::{Cli, Commands};

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

#[test]
fn test_next_subcommand_registered() {
    let cmd = Cli::command();
    let names: Vec<_> = cmd.get_subcommands().map(|c| c.get_name()).collect();
    assert!(
        names.contains(&"next"),
        "next subcommand must be registered"
    );
}

#[test]
fn test_next_parses_repo_flag() {
    let cli = Cli::try_parse_from(["glitchlab", "next", "--repo", "/tmp"]).unwrap();
    match cli.command {
        Commands::Next {
            repo,
            allow_core,
            auto_approve,
            test,
            verbose,
        } => {
            assert_eq!(repo, std::path::PathBuf::from("/tmp"));
            assert!(!allow_core);
            assert!(!auto_approve);
            assert!(test.is_none());
            assert!(!verbose);
        }
        _ => panic!("expected Commands::Next"),
    }
}

#[test]
fn test_next_parses_all_flags() {
    let cli = Cli::try_parse_from([
        "glitchlab",
        "next",
        "--repo",
        "/tmp",
        "--allow-core",
        "--auto-approve",
        "--test",
        "cargo test",
        "--verbose",
    ])
    .unwrap();
    match cli.command {
        Commands::Next {
            repo,
            allow_core,
            auto_approve,
            test,
            verbose,
        } => {
            assert_eq!(repo, std::path::PathBuf::from("/tmp"));
            assert!(allow_core);
            assert!(auto_approve);
            assert_eq!(test.as_deref(), Some("cargo test"));
            assert!(verbose);
        }
        _ => panic!("expected Commands::Next"),
    }
}
