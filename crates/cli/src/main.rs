use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};

mod commands;
#[cfg(test)]
mod main_test;

const LONG_VERSION: &str = concat!(
    env!("CARGO_PKG_VERSION"),
    " (",
    env!("GIT_HASH"),
    " ",
    env!("BUILD_DATE"),
    ")"
);

#[derive(Parser)]
#[command(
    name = "glitchlab",
    about = "The Agentic Dev Engine — Build Weird. Ship Clean.",
    version,
    long_version = LONG_VERSION,
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Run a single task
    Run {
        /// Path to the target repository
        #[arg(long)]
        repo: PathBuf,

        /// GitHub issue number
        #[arg(long)]
        issue: Option<u32>,

        /// Use local task file (.glitchlab/tasks/next.yaml)
        #[arg(long)]
        local_task: bool,

        /// Path to a specific task YAML file
        #[arg(long)]
        task_file: Option<PathBuf>,

        /// Inline objective (alternative to --issue/--local-task/--task-file)
        #[arg(long)]
        objective: Option<String>,

        /// Allow modifications to protected paths
        #[arg(long)]
        allow_core: bool,

        /// Skip human intervention gates
        #[arg(long)]
        auto_approve: bool,

        /// Override test command
        #[arg(long, short)]
        test: Option<String>,

        /// Enable verbose logging
        #[arg(long, short)]
        verbose: bool,
    },

    /// Interactive mode — describe what you want
    Interactive {
        /// Path to the target repository
        #[arg(long)]
        repo: PathBuf,

        /// Allow modifications to protected paths
        #[arg(long)]
        allow_core: bool,

        /// Skip human intervention gates
        #[arg(long)]
        auto_approve: bool,

        /// Override test command
        #[arg(long, short)]
        test: Option<String>,

        /// Enable verbose logging
        #[arg(long, short)]
        verbose: bool,
    },

    /// Run multiple tasks from a backlog with cumulative budget tracking
    Batch {
        /// Path to the target repository
        #[arg(long)]
        repo: PathBuf,

        /// Total dollar budget for all tasks combined
        #[arg(long)]
        budget: f64,

        /// Path to tasks YAML file (default: .glitchlab/tasks/backlog.yaml)
        #[arg(long)]
        tasks_file: Option<PathBuf>,

        /// Skip human intervention gates
        #[arg(long)]
        auto_approve: bool,

        /// Override test command
        #[arg(long, short)]
        test: Option<String>,

        /// Quality gate command to run between tasks (e.g. "cargo test --workspace")
        #[arg(long)]
        quality_gate: Option<String>,

        /// Stop on first task failure
        #[arg(long)]
        stop_on_failure: bool,

        /// Show task information without executing (dry run)
        #[arg(long)]
        dry_run: bool,

        /// Enable verbose logging
        #[arg(long, short)]
        verbose: bool,
    },

    /// Initialize GLITCHLAB in a repository
    Init {
        /// Path to the repository to initialize
        path: PathBuf,
    },

    /// Check configuration, API keys, and tools
    Status {
        /// Path to the repository (optional)
        #[arg(long)]
        repo: Option<PathBuf>,
    },

    /// View task history
    History {
        /// Path to the repository
        #[arg(long)]
        repo: PathBuf,

        /// Number of entries to show
        #[arg(long, default_value = "10")]
        count: usize,

        /// Show aggregate statistics
        #[arg(long)]
        stats: bool,
    },

    /// Print version information
    Version,

    /// View live dashboard
    Dashboard {
        /// Path to the repository
        #[arg(long, default_value = ".")]
        repo: PathBuf,

        /// Port to run the dashboard on
        #[arg(long, default_value = "3000")]
        port: u16,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Run {
            repo,
            issue,
            local_task,
            task_file,
            objective,
            allow_core,
            auto_approve,
            test,
            verbose,
        } => {
            commands::setup_logging(verbose);
            commands::run::execute(commands::run::RunArgs {
                repo: &repo,
                issue,
                local_task,
                task_file: task_file.as_deref(),
                objective: objective.as_deref(),
                allow_core,
                auto_approve,
                test: test.as_deref(),
            })
            .await
        }
        Commands::Interactive {
            repo,
            allow_core,
            auto_approve,
            test,
            verbose,
        } => {
            commands::setup_logging(verbose);
            commands::interactive::execute(&repo, allow_core, auto_approve, test.as_deref()).await
        }
        Commands::Batch {
            repo,
            budget,
            tasks_file,
            auto_approve,
            test,
            quality_gate,
            stop_on_failure,
            dry_run,
            verbose,
        } => {
            commands::setup_logging(verbose);
            commands::batch::execute(commands::batch::BatchArgs {
                repo: &repo,
                budget,
                tasks_file: tasks_file.as_deref(),
                auto_approve,
                test: test.as_deref(),
                quality_gate: quality_gate.as_deref(),
                stop_on_failure,
                dry_run,
            })
            .await
        }
        Commands::Init { path } => commands::init::execute(&path).await,
        Commands::Status { repo } => commands::status::execute(repo.as_deref()).await,
        Commands::History { repo, count, stats } => {
            commands::history::execute(&repo, count, stats).await
        }
        Commands::Version => commands::version::execute().await,
        Commands::Dashboard { repo, port } => {
            commands::setup_logging(false);
            commands::dashboard::execute(commands::dashboard::DashboardArgs { repo, port }).await
        }
    }
}
