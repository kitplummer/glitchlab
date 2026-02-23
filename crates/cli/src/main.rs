use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};

mod commands;

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

    /// Run multiple tasks in parallel
    Batch {
        /// Path to the target repository
        #[arg(long)]
        repo: PathBuf,

        /// Directory containing task YAML files
        #[arg(long)]
        tasks_dir: Option<PathBuf>,

        /// Maximum concurrent tasks
        #[arg(long, default_value = "3")]
        workers: usize,

        /// Allow modifications to protected paths
        #[arg(long)]
        allow_core: bool,

        /// Override test command
        #[arg(long, short)]
        test: Option<String>,

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
        Commands::Batch { repo, verbose, .. } => {
            commands::setup_logging(verbose);
            commands::batch::execute(&repo).await
        }
        Commands::Init { path } => commands::init::execute(&path).await,
        Commands::Status { repo } => commands::status::execute(repo.as_deref()).await,
        Commands::History { repo, count, stats } => {
            commands::history::execute(&repo, count, stats).await
        }
        Commands::Version => commands::version::execute().await,
    }
}
