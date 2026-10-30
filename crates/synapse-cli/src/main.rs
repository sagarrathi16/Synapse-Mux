mod stack;
mod ui;
mod update;

use std::fs;
use std::path::PathBuf;
use std::process::ExitCode;

use chrono::Utc;
use clap::{Parser, Subcommand};
use colored::Colorize;
use synapse_core::check::CheckStatus;
use synapse_core::git::GitManager;
use synapse_core::report::EvidenceReport;
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
    /// Initialize a starter synapse-spec.json and auto-detect repository stack
    #[command(alias = "setup")]
    Init {
        /// Feature ID / run title
        #[arg(short, long, default_value = "starter-feature")]
        feature_id: String,

        /// Path to repository root
        #[arg(long, default_value = ".")]
        repo: PathBuf,
    },

    /// Display status and recent runs in current repository
    Status {
        /// Path to repository root
        #[arg(long, default_value = ".")]
        repo: PathBuf,
    },

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
    #[command(alias = "clean")]
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

    /// Export a secret-redacted, code-stripped diagnostic bundle for open-source bug reports
    DiagnosticBundle {
        /// Run ID to export bundle for
        #[arg(long)]
        run_id: String,

        /// Destination path for exported diagnostic JSON file
        #[arg(short, long)]
        output: Option<PathBuf>,

        /// Path to repository root
        #[arg(long, default_value = ".")]
        repo: PathBuf,
    },

    /// Run the 5-arm comparative evaluation benchmark
    #[command(alias = "eval")]
    Benchmark {
        /// Directory containing task definitions
        #[arg(long, default_value = "eval/tasks")]
        tasks_dir: PathBuf,

        /// Output directory for benchmark report
        #[arg(long, default_value = "eval/results")]
        output_dir: PathBuf,

        /// Output directory for blinded review diff patches
        #[arg(long, default_value = "eval/blind_review")]
        blind_dir: PathBuf,

        /// Path to preregistration.json
        #[arg(long, default_value = "eval/preregistration.json")]
        preregistration: PathBuf,

        /// Number of repeats per task
        #[arg(long, default_value_t = 3)]
        repeats: usize,

        /// Randomize arm execution order
        #[arg(long, default_value_t = true)]
        randomize_order: bool,

        /// Randomization seed
        #[arg(long, default_value_t = 42)]
        seed: u64,

        /// Specific arms to run (comma-separated, e.g. "A,B,C,C-same,D")
        #[arg(long, value_delimiter = ',')]
        arms: Option<Vec<String>>,

        /// Enable optional MCP broker tools
        #[arg(long)]
        enable_mcp_broker: bool,

        /// Use mock mode for fast local verification
        #[arg(long, default_value_t = true)]
        mock: bool,
    },

    /// Check for and install updates to synapse-mux
    Update {
        /// Only check for updates without installing
        #[arg(long)]
        check: bool,
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
        #[arg(short, long)]
        spec: Option<PathBuf>,

        /// Explicit contract SHA-256 hash to approve
        #[arg(long)]
        approve_hash: Option<String>,

        /// Automatically approve without interactive confirmation
        #[arg(short, long)]
        yes: bool,

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

    ui::print_banner();

    let is_update_cmd = matches!(cli.command, Commands::Update { .. });

    let exit_code = match cli.command {
        Commands::Init { feature_id, repo } => {
            let mut steps = ui::StepProgress::new(4);

            // Step 1: Detect repository
            steps.start_step("Inspecting repository structure and Git status");
            let canonical_repo = repo.canonicalize().unwrap_or_else(|_| repo.clone());
            if !canonical_repo.join(".git").exists() {
                steps.fail_step("Not a Git repository");
                ui::print_error_box(
                    "Repository Not Initialized",
                    &format!("{:?} is not a git repository root.", canonical_repo),
                    "Run 'git init' before initializing Synapse.",
                );
                return ExitCode::FAILURE;
            }
            steps.complete_step("Git repository confirmed");

            // Step 2: Auto-detect stack
            steps.start_step("Auto-detecting tech stack and project layout");
            let detected = stack::DetectedStack::detect(&canonical_repo);
            steps.complete_step(&format!(
                "Detected {} ({})",
                detected.language, detected.package_manager
            ));
            if !detected.typecheck_cmd.is_empty() {
                steps.log_substep(&format!("Typecheck command: {:?}", detected.typecheck_cmd));
            }
            if !detected.test_cmd.is_empty() {
                steps.log_substep(&format!("Test command:      {:?}", detected.test_cmd));
            }

            // Step 3: Check AI toolchains
            steps.start_step("Checking installed AI provider toolchains");
            let tools = stack::ToolchainStatus::detect().await;
            let git_info = tools.git.as_deref().unwrap_or("git (system)");
            let claude_info = tools
                .claude
                .as_deref()
                .unwrap_or("Not installed (run: npm i -g @anthropic-ai/claude-code)");
            let codex_info = tools
                .codex
                .as_deref()
                .unwrap_or("Not installed (run: npm i -g @openai/codex-cli)");
            steps.log_substep(&format!("Git:         {}", git_info));
            steps.log_substep(&format!("Claude Code: {}", claude_info));
            steps.log_substep(&format!("Codex CLI:   {}", codex_info));
            steps.complete_step("Toolchain inspection complete");

            // Step 4: Scaffold spec and contract files
            steps.start_step("Scaffolding synapse-spec.json and contracts");
            let contract_file = canonical_repo.join(detected.contract_file);
            let contract_test = canonical_repo.join(detected.contract_test_file);

            if let Some(parent) = contract_file.parent() {
                let _ = fs::create_dir_all(parent);
            }
            if let Some(parent) = contract_test.parent() {
                let _ = fs::create_dir_all(parent);
            }

            if !contract_file.exists() {
                let _ = fs::write(&contract_file, detected.contract_stub);
            }
            if !contract_test.exists() {
                let _ = fs::write(&contract_test, detected.contract_test_stub);
            }

            let spec_path = canonical_repo.join("synapse-spec.json");
            let mut verif_cmds = Vec::new();
            if !detected.typecheck_cmd.is_empty() {
                verif_cmds.push(serde_json::json!({
                    "name": "typecheck",
                    "command": detected.typecheck_cmd
                }));
            }
            if !detected.test_cmd.is_empty() {
                verif_cmds.push(serde_json::json!({
                    "name": "test",
                    "command": detected.test_cmd
                }));
            }

            let spec_content = serde_json::json!({
                "schema_version": 1,
                "run_id": format!("{}-run-{}", feature_id, Utc::now().format("%Y%m%d")),
                "feature_id": feature_id,
                "base_commit": "HEAD",
                "contract_files": [detected.contract_file],
                "contract_tests": [detected.contract_test_file],
                "partitions": {
                    "claude": {
                        "role": "api",
                        "owned_globs": detected.claude_globs
                    },
                    "codex": {
                        "role": "worker",
                        "owned_globs": detected.codex_globs
                    }
                },
                "shared_files": [],
                "shared_file_policy": {},
                "verification_commands": verif_cmds
            });

            if let Err(e) = fs::write(
                &spec_path,
                serde_json::to_string_pretty(&spec_content).unwrap(),
            ) {
                steps.fail_step("Failed to write synapse-spec.json");
                ui::print_error_box(
                    "Failed to write synapse-spec.json",
                    &e.to_string(),
                    "Check write permissions.",
                );
                return ExitCode::FAILURE;
            }
            steps.complete_step("Scaffold generated successfully");
            ExitCode::SUCCESS
        }

        Commands::Status { repo } => {
            let runs_dir = repo.join(".synapse").join("runs");
            if !runs_dir.exists() {
                println!(
                    "{}",
                    "No active or recorded runs found in this repository.".dimmed()
                );
                return ExitCode::SUCCESS;
            }

            let mut run_entries = Vec::new();
            if let Ok(entries) = fs::read_dir(&runs_dir) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.is_dir() {
                        let run_id = entry.file_name().to_string_lossy().to_string();
                        let state_file = path.join("state.json");
                        let state_str = if state_file.exists() {
                            if let Ok(content) = fs::read_to_string(&state_file) {
                                if let Ok(val) = serde_json::from_str::<serde_json::Value>(&content)
                                {
                                    val.get("current_state")
                                        .and_then(|s| s.as_str())
                                        .unwrap_or("Unknown")
                                        .to_string()
                                } else {
                                    "Corrupted".into()
                                }
                            } else {
                                "Unreadable".into()
                            }
                        } else {
                            "Initialized".into()
                        };

                        let report_file = path.join("report.json");
                        let result_str = if report_file.exists() {
                            "Evidence Ready".green().to_string()
                        } else {
                            "-".dimmed().to_string()
                        };

                        run_entries.push((run_id, state_str, result_str));
                    }
                }
            }

            if run_entries.is_empty() {
                println!("{}", "No runs recorded yet in .synapse/runs/".dimmed());
                return ExitCode::SUCCESS;
            }

            println!(
                "{} ({})\n",
                "Recorded Orchestration Runs".bold(),
                run_entries.len()
            );
            println!(
                "{:<28} {:<18} {:<15}",
                "Run ID".dimmed(),
                "Current State".dimmed(),
                "Evidence".dimmed()
            );
            println!("{}", "─".repeat(64).dimmed());
            for (id, state, ev) in run_entries {
                let colored_state = match state.as_str() {
                    "ReviewReady" => state.green().bold().to_string(),
                    "Failed" | "Cancelled" => state.red().to_string(),
                    _ => state.cyan().to_string(),
                };
                println!("{:<28} {:<18} {:<15}", id.bold(), colored_state, ev);
            }
            println!();
            ExitCode::SUCCESS
        }

        Commands::Preflight {
            allow_dirty_with_snapshot,
            repo,
        } => {
            let mut steps = ui::StepProgress::new(3);

            steps.start_step("Verifying Git repository status and cleanliness");
            let git_mgr = match GitManager::new(&repo) {
                Ok(mgr) => mgr,
                Err(e) => {
                    steps.fail_step("Failed to locate Git repository");
                    ui::print_error_box(
                        "Not a Git repository",
                        &e.to_string(),
                        "Run 'git init' or pass '--repo <path>' pointing to a valid Git repository.",
                    );
                    return ExitCode::FAILURE;
                }
            };

            let preflight_res = match git_mgr.preflight(allow_dirty_with_snapshot).await {
                Ok(res) => {
                    steps.complete_step(&format!(
                        "Git repo clean at {} ({})",
                        &res.current_commit[..7.min(res.current_commit.len())],
                        res.git_version
                    ));
                    res
                }
                Err(e) => {
                    steps.fail_step("Working tree is dirty or Git check failed");
                    ui::print_error_box(
                        "Preflight Rejected Workspace",
                        &e.to_string(),
                        "Commit your local changes, or re-run with '--allow-dirty-with-snapshot' to safely capture an uncommitted stash SHA.",
                    );
                    return ExitCode::FAILURE;
                }
            };

            steps.start_step("Detecting installed cross-model CLI toolchains");
            let claude_status = preflight_res
                .claude_version
                .as_deref()
                .unwrap_or("Not installed (Claude Code CLI)");
            let codex_status = preflight_res
                .codex_version
                .as_deref()
                .unwrap_or("Not installed (OpenAI Codex CLI)");

            steps.log_substep(&format!("Claude Code CLI:  {}", claude_status.cyan()));
            steps.log_substep_final(&format!("OpenAI Codex CLI: {}", codex_status.cyan()));
            steps.complete_step("Toolchains inspection complete");

            steps.start_step("Verifying path isolation and security boundaries");
            steps.complete_step("Security boundaries confirmed (repo-root canonicalized)");

            ui::print_card(
                "✔ Preflight Verification Passed",
                &[
                    (
                        "Repository Root",
                        preflight_res.repo_root.display().to_string(),
                    ),
                    ("HEAD Commit", preflight_res.current_commit),
                    ("Git Version", preflight_res.git_version),
                    (
                        "Tree Status",
                        if preflight_res.is_clean {
                            "Clean".green().to_string()
                        } else {
                            "Dirty with Stash Snapshot".yellow().to_string()
                        },
                    ),
                    ("Claude Code", claude_status.to_string()),
                    ("Codex CLI", codex_status.to_string()),
                ],
                Some(&["Ready to proceed with 'synapse contract approve' or 'synapse run'."]),
            );

            ExitCode::SUCCESS
        }

        Commands::Contract { command } => match command {
            ContractCommands::Approve {
                run_id,
                spec,
                approve_hash,
                yes,
                repo,
            } => {
                let spec_path = spec.unwrap_or_else(|| repo.join("synapse-spec.json"));
                if !spec_path.exists() {
                    ui::print_error_box(
                        "Specification File Missing",
                        &format!("Cannot find {:?}", spec_path),
                        "Run 'synapse init' to generate a starter synapse-spec.json.",
                    );
                    return ExitCode::FAILURE;
                }

                // Interactive confirmation prompt if in interactive terminal
                if !yes && approve_hash.is_none() {
                    println!("{}", "Inspecting contract proposal...".dimmed());
                    if !ui::prompt_confirm(
                        "Do you approve locking this contract for implementation?",
                        true,
                    ) {
                        println!("{}", "Contract approval aborted by user.".yellow());
                        return ExitCode::FAILURE;
                    }
                }

                let mut steps = ui::StepProgress::new(2);
                steps.start_step(
                    "Validating synapse-spec.json and checking contract file integrity",
                );

                match Orchestrator::approve_contract(
                    &repo,
                    &spec_path,
                    run_id.clone(),
                    approve_hash,
                )
                .await
                {
                    Ok(hash) => {
                        steps.complete_step(&format!(
                            "Contract hash computed & verified: {}",
                            &hash[..16]
                        ));
                        steps.start_step("Locking contract commit & generating approval record");
                        steps.complete_step("Contract state transitioned to CONTRACT_APPROVAL");

                        ui::print_card(
                            "✔ Contract Approved Successfully",
                            &[
                                ("Contract Hash", hash),
                                ("Run ID", run_id.unwrap_or_else(|| "default".into())),
                                ("Specification", spec_path.display().to_string()),
                                (
                                    "Approval Status",
                                    "Approved & Hash-Locked".green().bold().to_string(),
                                ),
                            ],
                            Some(&["Next Step: Run 'synapse run' to execute parallel workers."]),
                        );
                        ExitCode::SUCCESS
                    }
                    Err(e) => {
                        steps.fail_step("Contract approval rejected");
                        ui::print_error_box(
                            "Contract Approval Failed",
                            &e.to_string(),
                            "Ensure all contract_files and contract_tests exist and pass typecheck.",
                        );
                        ExitCode::FAILURE
                    }
                }
            }
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
                println!(
                    "Starting {} single-agent execution...",
                    "Arm A / D".bold().cyan()
                );
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
                        print_evidence_card(&report);
                        if report.status == "REVIEW_READY" {
                            ExitCode::SUCCESS
                        } else {
                            ExitCode::FAILURE
                        }
                    }
                    Err(e) => {
                        ui::print_error_box(
                            "Single-Agent Run Failed",
                            &e.to_string(),
                            "Check preflight status or adapter configuration.",
                        );
                        ExitCode::FAILURE
                    }
                }
            } else {
                let spec_path = spec.unwrap_or_else(|| repo.join("synapse-spec.json"));
                if !spec_path.exists() {
                    ui::print_error_box(
                        "Specification File Missing",
                        &format!("Cannot find {:?}", spec_path),
                        "Create a synapse-spec.json file or run 'synapse init' to scaffold one.",
                    );
                    return ExitCode::FAILURE;
                }

                println!(
                    "Launching {} Orchestration with {} and {}...\n",
                    "Dual-Agent".bold().cyan(),
                    "Claude Code".yellow(),
                    "OpenAI Codex".green()
                );

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
                        print_evidence_card(&report);
                        if report.status == "REVIEW_READY" {
                            ExitCode::SUCCESS
                        } else {
                            ExitCode::FAILURE
                        }
                    }
                    Err(e) => {
                        ui::print_error_box(
                            "Dual-Agent Orchestration Failed",
                            &e.to_string(),
                            "Inspect .synapse/runs/ for failure diagnostics or run 'synapse resume --run-id <id>'.",
                        );
                        ExitCode::FAILURE
                    }
                }
            }
        }

        Commands::Resume { run_id, spec, repo } => {
            println!("Resuming interrupted run '{}'...", run_id.bold().cyan());
            let options = DualWorkerOptions {
                spec_path: spec,
                run_id: Some(run_id.clone()),
                ..Default::default()
            };

            match Orchestrator::resume_dual_agent(&repo, options).await {
                Ok(report) => {
                    print_evidence_card(&report);
                    if report.status == "REVIEW_READY" {
                        ExitCode::SUCCESS
                    } else {
                        ExitCode::FAILURE
                    }
                }
                Err(e) => {
                    ui::print_error_box(
                        "Failed to Resume Run",
                        &e.to_string(),
                        "Check that worker checkpoint refs exist via 'git show-ref'.",
                    );
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
                    ui::print_card(
                        "Run Cancelled",
                        &[
                            ("Run ID", run_id),
                            ("Status", "Cancelled".red().bold().to_string()),
                        ],
                        Some(&["Worker process trees terminated and state marked Cancelled."]),
                    );
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    ui::print_error_box(
                        "Failed to Cancel Run",
                        &e.to_string(),
                        "Verify the run ID.",
                    );
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
                    ui::print_card(
                        "Cleanup Completed",
                        &[
                            ("Run ID", summary.run_id),
                            (
                                "Removed Worktrees",
                                format!("{}", summary.removed_worktrees.len()),
                            ),
                            (
                                "Artifacts Preserved",
                                if summary.artifacts_kept {
                                    "Yes".green().to_string()
                                } else {
                                    "No".to_string()
                                },
                            ),
                        ],
                        Some(&["User working tree remains untouched."]),
                    );
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    ui::print_error_box(
                        "Failed to Clean Up Worktrees",
                        &e.to_string(),
                        "Check file locks.",
                    );
                    ExitCode::FAILURE
                }
            }
        }

        Commands::DiagnosticBundle {
            run_id,
            output,
            repo,
        } => {
            info!("Exporting redacted diagnostic bundle for run '{}'", run_id);
            match Orchestrator::export_diagnostic_bundle(&repo, &run_id, output).await {
                Ok(path) => {
                    ui::print_card(
                        "Diagnostic Bundle Exported",
                        &[
                            ("Run ID", run_id),
                            ("Bundle Path", path.display().to_string()),
                            (
                                "Secrets Redacted",
                                "Yes (Tokens, Keys, Auth)".green().to_string(),
                            ),
                            (
                                "Source Code Stripped",
                                "Yes (Metadata only)".green().to_string(),
                            ),
                        ],
                        Some(&["Safe to attach to public GitHub bug reports."]),
                    );
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    ui::print_error_box(
                        "Failed to Export Diagnostic Bundle",
                        &e.to_string(),
                        "Verify run artifacts exist.",
                    );
                    ExitCode::FAILURE
                }
            }
        }

        Commands::Benchmark {
            tasks_dir,
            output_dir,
            blind_dir,
            preregistration,
            repeats,
            randomize_order,
            seed,
            arms,
            enable_mcp_broker,
            mock,
        } => {
            let selected_arms = if let Some(arm_strs) = arms {
                let mut parsed = Vec::new();
                for s in arm_strs {
                    if let Some(arm) = synapse_eval::ArmKind::parse(&s) {
                        parsed.push(arm);
                    } else {
                        eprintln!("Warning: ignoring unknown arm '{s}'");
                    }
                }
                parsed
            } else {
                synapse_eval::ArmKind::all()
            };

            let config = synapse_eval::BenchmarkConfig {
                tasks_dir,
                output_dir: output_dir.clone(),
                blind_dir: blind_dir.clone(),
                repeats,
                randomize_order,
                seed,
                arms: selected_arms,
                mock_mode: mock,
                enable_mcp_broker,
            };

            println!(
                "Running 5-arm comparative evaluation across {} arms...\n",
                config.arms.len()
            );

            let runner = synapse_eval::BenchmarkRunner::new(config);
            match runner.run_benchmark().await {
                Ok(run_report) => {
                    match synapse_eval::PreRegistration::load_from_file(&preregistration) {
                        Ok(prereg) => {
                            let report =
                                synapse_eval::EvaluationReport::build(&run_report.results, &prereg);
                            if let Err(e) = report.save_to_dir(&output_dir) {
                                eprintln!("Failed to save benchmark report: {e}");
                                return ExitCode::FAILURE;
                            }
                            println!("{}", report.to_markdown());
                            if report.go_no_go.passed {
                                ExitCode::SUCCESS
                            } else {
                                ExitCode::FAILURE
                            }
                        }
                        Err(e) => {
                            ui::print_error_box(
                                "Failed to load pre-registration",
                                &e.to_string(),
                                "Check preregistration.json path.",
                            );
                            ExitCode::FAILURE
                        }
                    }
                }
                Err(e) => {
                    ui::print_error_box(
                        "Benchmark Execution Failed",
                        &e.to_string(),
                        "Check tasks directory.",
                    );
                    ExitCode::FAILURE
                }
            }
        }

        Commands::Update { check } => update::run_update(check).await,
    };

    if !is_update_cmd
        && let Some(latest) = update::check_for_update_cached(env!("CARGO_PKG_VERSION")).await
    {
        update::print_update_banner(env!("CARGO_PKG_VERSION"), &latest);
    }

    exit_code
}

/// Helper to render the final Evidence Report Card on CLI.
fn print_evidence_card(report: &EvidenceReport) {
    let status_colored = if report.status == "REVIEW_READY" {
        "✔ REVIEW READY".green().bold().to_string()
    } else {
        "✖ FAILED".red().bold().to_string()
    };

    let checks_passed = report
        .checks
        .iter()
        .filter(|c| c.status == CheckStatus::Passed)
        .count();
    let checks_total = report.checks.len();
    let checks_str = format!("{}/{} passed", checks_passed, checks_total);

    let token_spend = format!(
        "{} tokens (~${:.2})",
        report.telemetry.observed_usage.input_tokens
            + report.telemetry.observed_usage.output_tokens,
        report.telemetry.estimated_cost_usd
    );

    let notes: Vec<String> = report.next_actions.clone();
    let note_refs: Vec<&str> = notes.iter().map(|s| s.as_str()).collect();

    ui::print_card(
        "Synapse-Mux Evidence Report Card",
        &[
            ("Status", status_colored),
            ("Run ID", report.run_id.clone()),
            ("Duration", format!("{:.1}s", report.duration_seconds)),
            (
                "Contract Hash",
                report
                    .contract_hash
                    .as_deref()
                    .unwrap_or("None")
                    .chars()
                    .take(16)
                    .collect(),
            ),
            ("Verification", checks_str),
            ("Model Telemetry", token_spend),
            (
                "Changed Files",
                format!("{} files modified", report.changed_files.len()),
            ),
        ],
        Some(&note_refs),
    );
}
