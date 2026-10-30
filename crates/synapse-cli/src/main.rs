use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use synapse_core::git::GitManager;
use synapse_core::runner::{Orchestrator, SingleWorkerOptions};
use tracing::{Level, info};
use tracing_subscriber::FmtSubscriber;

#[derive(Parser)]
#[command(
    name = "synapse",
    version,
    about = "Contract-First Cross-Model Agent Orchestrator"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,

    /// Verbose output logging
    #[arg(short, long, global = true)]
    verbose: bool,
}

#[derive(Subcommand)]
enum Commands {
    /// Verify repository, Git status, and installed CLI toolchains
    Preflight {
        /// Allow dirty working tree by capturing an uncommitted stash commit
        #[arg(long)]
        allow_dirty_with_snapshot: bool,

        /// Path to repository root (defaults to current directory)
        #[arg(long, default_value = ".")]
        repo: PathBuf,
    },

    /// Run orchestration workflow
    Run {
        /// Path to synapse-spec.json specification
        #[arg(short, long)]
        spec: Option<PathBuf>,

        /// Run in single-agent baseline mode (Arm A / Arm D scaffolding)
        #[arg(long, default_value_t = true)]
        single_agent: bool,

        /// Provider adapter to invoke: "claude" or "mock"
        #[arg(long, default_value = "claude")]
        adapter: String,

        /// Task or feature prompt
        #[arg(short, long)]
        prompt: Option<String>,

        /// Allow dirty working tree with recorded stash snapshot
        #[arg(long)]
        allow_dirty_with_snapshot: bool,

        /// Path to repository root
        #[arg(long, default_value = ".")]
        repo: PathBuf,

        /// Path to recorded JSONL event fixture for mock execution
        #[arg(long)]
        mock_fixture: Option<PathBuf>,

        /// Simulate worker failure with mock adapter
        #[arg(long)]
        mock_failure: bool,

        /// Simulate worker timeout with mock adapter
        #[arg(long)]
        mock_timeout: bool,
    },
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();

    let log_level = if cli.verbose {
        Level::DEBUG
    } else {
        Level::INFO
    };

    let subscriber = FmtSubscriber::builder()
        .with_max_level(log_level)
        .with_target(false)
        .finish();

    let _ = tracing::subscriber::set_global_default(subscriber);

    match cli.command {
        Commands::Preflight {
            allow_dirty_with_snapshot,
            repo,
        } => {
            info!("Running Synapse Preflight in {:?}", repo);
            let git_mgr = match GitManager::new(&repo) {
                Ok(mgr) => mgr,
                Err(e) => {
                    eprintln!("Error initializing Git manager: {e}");
                    return ExitCode::FAILURE;
                }
            };

            match git_mgr.preflight(allow_dirty_with_snapshot).await {
                Ok(res) => {
                    println!("\n=== Synapse Preflight Passed ===");
                    println!("Git Version:       {}", res.git_version);
                    println!("Repository Root:   {}", res.repo_root.display());
                    println!("HEAD Commit:       {}", res.current_commit);
                    println!(
                        "Working Tree:      {}",
                        if res.is_clean {
                            "Clean"
                        } else {
                            "Dirty (Stash Snapshot Created)"
                        }
                    );
                    if let Some(stash) = res.stash_snapshot_sha {
                        println!("Stash Snapshot:    {}", stash);
                    }
                    println!(
                        "Claude Code CLI:   {}",
                        res.claude_version
                            .unwrap_or_else(|| "Not found in PATH".into())
                    );
                    println!(
                        "OpenAI Codex CLI:  {}",
                        res.codex_version
                            .unwrap_or_else(|| "Not found in PATH".into())
                    );
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("\n=== Preflight Failed ===");
                    eprintln!("{e}");
                    ExitCode::FAILURE
                }
            }
        }
        Commands::Run {
            spec,
            single_agent,
            adapter,
            prompt,
            allow_dirty_with_snapshot,
            repo,
            mock_fixture,
            mock_failure,
            mock_timeout,
        } => {
            if !single_agent {
                eprintln!(
                    "Error: Multi-agent parallel mode is scheduled for Milestone 2. Pass --single-agent."
                );
                return ExitCode::FAILURE;
            }

            let options = SingleWorkerOptions {
                spec_path: spec,
                run_id: None,
                prompt,
                allow_dirty_with_snapshot,
                adapter_name: adapter,
                mock_fixture,
                mock_failure,
                mock_timeout,
                mock_file_writes: Vec::new(),
            };

            match Orchestrator::run_single_agent(&repo, options).await {
                Ok(report) => {
                    println!("\n{}", report.to_markdown_string());
                    if report.status == "REVIEW_READY" {
                        ExitCode::SUCCESS
                    } else {
                        ExitCode::FAILURE
                    }
                }
                Err(e) => {
                    eprintln!("\n=== Run Execution Failed ===");
                    eprintln!("{e}");
                    ExitCode::FAILURE
                }
            }
        }
    }
}
