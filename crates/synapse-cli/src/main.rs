use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use synapse_core::git::GitManager;
use synapse_core::runner::{DualWorkerOptions, Orchestrator, SingleWorkerOptions};
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

    /// Contract management and approval
    Contract {
        #[command(subcommand)]
        command: ContractCommands,
    },

    /// Run orchestration workflow
    Run {
        /// Path to synapse-spec.json specification
        #[arg(short, long)]
        spec: Option<PathBuf>,

        /// Run in single-agent baseline mode (Arms A/D)
        #[arg(long)]
        single_agent: bool,

        /// Adapter for Claude worker: "claude" or "mock"
        #[arg(long, default_value = "claude")]
        claude_adapter: String,

        /// Adapter for Codex worker: "codex" or "mock"
        #[arg(long, default_value = "codex")]
        codex_adapter: String,

        /// Task or feature prompt
        #[arg(short, long)]
        prompt: Option<String>,

        /// Allow dirty working tree with recorded stash snapshot
        #[arg(long)]
        allow_dirty_with_snapshot: bool,

        /// Path to repository root
        #[arg(long, default_value = ".")]
        repo: PathBuf,

        /// Explicit contract approval hash
        #[arg(long)]
        approve_hash: Option<String>,

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

    /// Resume an interrupted or checkpointed run
    Resume {
        /// Run ID to resume
        #[arg(long)]
        run_id: String,

        /// Path to synapse-spec.json specification
        #[arg(short, long, default_value = "synapse-spec.json")]
        spec: PathBuf,

        /// Path to repository root
        #[arg(long, default_value = ".")]
        repo: PathBuf,
    },

    /// Cancel an active run and mark it cancelled
    Cancel {
        /// Run ID to cancel
        #[arg(long)]
        run_id: String,

        /// Reason for cancellation
        #[arg(short, long)]
        reason: Option<String>,

        /// Path to repository root
        #[arg(long, default_value = ".")]
        repo: PathBuf,
    },

    /// Clean up generated worktrees safely
    Cleanup {
        /// Run ID to clean up
        #[arg(long)]
        run_id: String,

        /// Keep run artifacts in .synapse/runs/<run_id>/
        #[arg(long, default_value_t = true)]
        keep_artifacts: bool,

        /// Path to repository root
        #[arg(long, default_value = ".")]
        repo: PathBuf,
    },
}

#[derive(Subcommand)]
enum ContractCommands {
    /// Approve contract proposal for execution
    Approve {
        /// Run ID
        #[arg(long)]
        run_id: Option<String>,

        /// Path to synapse-spec.json
        #[arg(short, long, default_value = "synapse-spec.json")]
        spec: PathBuf,

        /// Explicit contract SHA-256 hash to approve
        #[arg(long)]
        approve_hash: Option<String>,

        /// Path to repository root
        #[arg(long, default_value = ".")]
        repo: PathBuf,
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
        Commands::Contract { command } => match command {
            ContractCommands::Approve {
                run_id,
                spec,
                approve_hash,
                repo,
            } => match Orchestrator::approve_contract(&repo, &spec, run_id, approve_hash).await {
                Ok(hash) => {
                    println!("\n=== Contract Approved Successfully ===");
                    println!("Contract Hash: {hash}");
                    println!("Status:        CONTRACT_APPROVAL");
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("\n=== Contract Approval Failed ===");
                    eprintln!("{e}");
                    ExitCode::FAILURE
                }
            },
        },
        Commands::Run {
            spec,
            single_agent,
            claude_adapter,
            codex_adapter,
            prompt,
            allow_dirty_with_snapshot,
            repo,
            approve_hash,
            mock_fixture,
            mock_failure,
            mock_timeout,
        } => {
            if single_agent {
                let options = SingleWorkerOptions {
                    spec_path: spec,
                    run_id: None,
                    prompt,
                    allow_dirty_with_snapshot,
                    adapter_name: claude_adapter,
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
            } else {
                let spec_path = spec.unwrap_or_else(|| repo.join("synapse-spec.json"));
                let options = DualWorkerOptions {
                    spec_path,
                    run_id: None,
                    allow_dirty_with_snapshot,
                    claude_adapter_name: claude_adapter,
                    codex_adapter_name: codex_adapter,
                    claude_prompt: prompt.clone(),
                    codex_prompt: prompt,
                    claude_mock_fixture: mock_fixture.clone(),
                    codex_mock_fixture: mock_fixture,
                    claude_mock_file_writes: Vec::new(),
                    codex_mock_file_writes: Vec::new(),
                    approve_hash,
                    ..Default::default()
                };

                match Orchestrator::run_dual_agent(&repo, options).await {
                    Ok(report) => {
                        println!("\n{}", report.to_markdown_string());
                        if report.status == "REVIEW_READY" {
                            ExitCode::SUCCESS
                        } else {
                            ExitCode::FAILURE
                        }
                    }
                    Err(e) => {
                        eprintln!("\n=== Dual-Agent Run Execution Failed ===");
                        eprintln!("{e}");
                        ExitCode::FAILURE
                    }
                }
            }
        }
        Commands::Resume { run_id, spec, repo } => {
            info!("Resuming run '{}'", run_id);
            let options = DualWorkerOptions {
                spec_path: spec,
                run_id: Some(run_id.clone()),
                ..Default::default()
            };

            match Orchestrator::resume_dual_agent(&repo, options).await {
                Ok(report) => {
                    println!("\n=== Run Resumed Successfully ===");
                    println!("{}", report.to_markdown_string());
                    if report.status == "REVIEW_READY" {
                        ExitCode::SUCCESS
                    } else {
                        ExitCode::FAILURE
                    }
                }
                Err(e) => {
                    eprintln!("\n=== Run Resume Failed ===");
                    eprintln!("{e}");
                    ExitCode::FAILURE
                }
            }
        }
        Commands::Cancel {
            run_id,
            reason,
            repo,
        } => {
            info!("Cancelling run '{}'", run_id);
            match Orchestrator::cancel_run(&repo, &run_id, reason).await {
                Ok(()) => {
                    println!("\nRun '{}' cancelled successfully.", run_id);
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("\n=== Failed to Cancel Run ===");
                    eprintln!("{e}");
                    ExitCode::FAILURE
                }
            }
        }
        Commands::Cleanup {
            run_id,
            keep_artifacts,
            repo,
        } => {
            info!("Cleaning up worktrees for run '{}'", run_id);
            match Orchestrator::cleanup_run(&repo, &run_id, keep_artifacts).await {
                Ok(summary) => {
                    println!("\n=== Cleanup Completed ===");
                    println!("Run ID:            {}", summary.run_id);
                    println!("Removed Worktrees: {}", summary.removed_worktrees.len());
                    println!("Artifacts Kept:    {}", summary.artifacts_kept);
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("\n=== Failed to Cleanup Run ===");
                    eprintln!("{e}");
                    ExitCode::FAILURE
                }
            }
        }
    }
}
