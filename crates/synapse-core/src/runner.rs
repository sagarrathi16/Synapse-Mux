use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use chrono::Utc;
use thiserror::Error;
use tokio::sync::mpsc;
use tracing::{info, warn};
use uuid::Uuid;

use crate::adapters::claude::ClaudeCodeAdapter;
use crate::adapters::mock::MockAdapter;
use crate::adapters::{AgentEvent, ObservedUsage, WorkerAdapter, WorkerRequest};
use crate::check::{CheckRunner, CheckStatus};
use crate::config::SynapseSpec;
use crate::git::{GitError, GitManager};
use crate::report::{EvidenceReport, TelemetryReport};
use crate::state::{RunState, RunStore, StateError, TransitionOutcome};

#[derive(Error, Debug)]
pub enum RunnerError {
    #[error("Git error: {0}")]
    Git(#[from] GitError),
    #[error("State machine error: {0}")]
    State(#[from] StateError),
    #[error("Config error: {0}")]
    Config(#[from] crate::config::ConfigError),
    #[error("Adapter error: {0}")]
    Adapter(#[from] crate::adapters::AdapterError),
    #[error("Serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Run failed: {0}")]
    ExecutionFailed(String),
}

#[derive(Debug, Clone)]
pub struct SingleWorkerOptions {
    pub spec_path: Option<PathBuf>,
    pub run_id: Option<String>,
    pub prompt: Option<String>,
    pub allow_dirty_with_snapshot: bool,
    pub adapter_name: String,
    pub mock_fixture: Option<PathBuf>,
    pub mock_failure: bool,
    pub mock_timeout: bool,
    pub mock_file_writes: Vec<(String, String)>,
}

impl Default for SingleWorkerOptions {
    fn default() -> Self {
        Self {
            spec_path: None,
            run_id: None,
            prompt: None,
            allow_dirty_with_snapshot: false,
            adapter_name: "claude".into(),
            mock_fixture: None,
            mock_failure: false,
            mock_timeout: false,
            mock_file_writes: Vec::new(),
        }
    }
}

pub struct Orchestrator;

impl Orchestrator {
    pub async fn run_single_agent(
        repo_root: impl AsRef<Path>,
        options: SingleWorkerOptions,
    ) -> Result<EvidenceReport, RunnerError> {
        let start_time = Instant::now();
        let started_at = Utc::now();
        let repo_root = repo_root.as_ref();

        // 1. Resolve Spec or default configuration
        let (spec, spec_hash) = if let Some(ref path) = options.spec_path {
            let s = SynapseSpec::from_file(path)?;
            s.validate(repo_root)?;
            let h = s.compute_contract_hash(repo_root)?;
            (Some(s), Some(h))
        } else {
            (None, None)
        };

        let run_id = options.run_id.unwrap_or_else(|| {
            spec.as_ref()
                .map(|s| s.run_id.clone())
                .unwrap_or_else(|| format!("run-{}", Uuid::new_v4()))
        });

        let feature_id = spec
            .as_ref()
            .map(|s| s.feature_id.clone())
            .unwrap_or_else(|| "single-agent-task".into());

        // 2. Initialize Run Store & State Machine
        let store = RunStore::new(repo_root, &run_id)?;
        let git_mgr = GitManager::new(repo_root)?;

        info!("Starting single-agent orchestration run '{}'", run_id);

        // 3. State: PREFLIGHT
        let preflight = match git_mgr.preflight(options.allow_dirty_with_snapshot).await {
            Ok(p) => p,
            Err(e) => {
                store.initialize(None, spec_hash.clone())?;
                store.transition_to(
                    RunState::Failed,
                    vec![],
                    TransitionOutcome::Failure(format!("Preflight check failed: {e}")),
                    Some("Preflight rejected workspace state".into()),
                )?;
                return Err(RunnerError::Git(e));
            }
        };

        let base_commit = spec
            .as_ref()
            .map(|s| s.base_commit.clone())
            .unwrap_or_else(|| preflight.current_commit.clone());

        store.initialize(Some(base_commit.clone()), spec_hash.clone())?;

        let preflight_json = serde_json::to_string_pretty(&preflight)?;
        store.save_artifact("preflight.json", preflight_json.as_bytes())?;

        // 4. State: IMPLEMENTATION
        store.transition_to(
            RunState::Implementation,
            vec!["preflight.json".into()],
            TransitionOutcome::Success,
            Some("Preflight passed, provisioning worktree".into()),
        )?;

        let worktree_dir = match git_mgr
            .create_worktree(&run_id, "worker-single", &base_commit)
            .await
        {
            Ok(dir) => dir,
            Err(e) => {
                store.transition_to(
                    RunState::Failed,
                    vec![],
                    TransitionOutcome::Failure(format!("Failed to create worktree: {e}")),
                    Some("Worktree creation failure".into()),
                )?;
                return Err(RunnerError::Git(e));
            }
        };

        let events_log_path = store.artifacts_dir().join("events.jsonl");
        let (tx, mut rx) = mpsc::channel::<AgentEvent>(100);

        // Spawn async event persistence loop
        let events_log_clone = events_log_path.clone();
        let logger_task = tokio::spawn(async move {
            let mut file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&events_log_clone)
                .ok();

            while let Some(event) = rx.recv().await {
                if let Some(ref mut f) = file
                    && let Ok(json_line) = serde_json::to_string(&event)
                {
                    let _ = writeln!(f, "{json_line}");
                }
            }
        });

        // Select adapter
        let adapter: Arc<dyn WorkerAdapter> = match options.adapter_name.as_str() {
            "mock" => {
                let mut mock = MockAdapter::new()
                    .with_failure(options.mock_failure)
                    .with_timeout(options.mock_timeout);
                if let Some(ref fixture) = options.mock_fixture {
                    mock = mock.with_fixture(fixture);
                }
                for (path, content) in options.mock_file_writes {
                    mock = mock.with_file_write(path, content);
                }
                Arc::new(mock)
            }
            _ => Arc::new(ClaudeCodeAdapter::default()),
        };

        let prompt = options
            .prompt
            .unwrap_or_else(|| "Implement the requested contract and changes.".into());

        let timeout_seconds = spec
            .as_ref()
            .map(|s| s.limits.worker_timeout_seconds)
            .unwrap_or(600);

        let worker_req = WorkerRequest {
            run_id: run_id.clone(),
            worker_name: "worker-single".into(),
            prompt,
            worktree_dir: worktree_dir.clone(),
            timeout_seconds,
            max_turns: Some(30),
            max_budget_usd: Some(5.0),
            model: Some("claude-3-7-sonnet".into()),
        };

        let worker_result = adapter.run_worker(worker_req, tx).await;
        let _ = logger_task.await;

        let (worker_ok, observed_usage, estimated_cost, worker_err_msg) = match worker_result {
            Ok(res) => (
                res.success,
                res.observed_usage,
                res.estimated_cost_usd,
                if res.success {
                    None
                } else {
                    res.final_message.or_else(|| Some("Worker failed".into()))
                },
            ),
            Err(e) => (
                false,
                ObservedUsage::default(),
                0.0,
                Some(format!("Adapter error: {e}")),
            ),
        };

        // 5. State: CHECKPOINT
        store.transition_to(
            RunState::Checkpoint,
            vec!["events.jsonl".into()],
            if worker_ok {
                TransitionOutcome::Success
            } else {
                TransitionOutcome::Warning(worker_err_msg.clone().unwrap_or_default())
            },
            Some("Worker completed, capturing checkpoint snapshot".into()),
        )?;

        let checkpoint = match git_mgr
            .capture_checkpoint(
                &worktree_dir,
                &run_id,
                "worker-single",
                &base_commit,
                spec_hash.as_deref(),
            )
            .await
        {
            Ok(cp) => cp,
            Err(e) => {
                let _ = git_mgr.remove_worktree(&worktree_dir).await;
                store.transition_to(
                    RunState::Failed,
                    vec![],
                    TransitionOutcome::Failure(format!("Failed to capture checkpoint: {e}")),
                    None,
                )?;
                return Err(RunnerError::Git(e));
            }
        };

        let checkpoint_json = serde_json::to_string_pretty(&checkpoint)?;
        store.save_artifact("checkpoint.json", checkpoint_json.as_bytes())?;
        store.save_artifact("patch.diff", checkpoint.unified_diff.as_bytes())?;

        // 6. State: VERIFICATION
        store.transition_to(
            RunState::Verification,
            vec!["checkpoint.json".into(), "patch.diff".into()],
            TransitionOutcome::Success,
            Some("Running configured verification checks".into()),
        )?;

        let verification_cmds = spec
            .as_ref()
            .map(|s| s.verification_commands.clone())
            .unwrap_or_default();

        let check_results = CheckRunner::run_all(&verification_cmds, &worktree_dir).await;

        let all_required_passed = check_results
            .iter()
            .filter(|c| c.required)
            .all(|c| c.status == CheckStatus::Passed);

        let checks_json = serde_json::to_string_pretty(&check_results)?;
        store.save_artifact("checks.json", checks_json.as_bytes())?;

        // 7. Safe Teardown of Worktree
        if let Err(e) = git_mgr.remove_worktree(&worktree_dir).await {
            warn!(
                "Failed to cleanly remove worktree at {:?}: {}",
                worktree_dir, e
            );
        }

        let completed_at = Utc::now();
        let duration_seconds = start_time.elapsed().as_secs_f64();

        // 8. Determine final state and build evidence report
        let overall_success = worker_ok && all_required_passed;
        let final_status = if overall_success {
            "REVIEW_READY".to_string()
        } else {
            "FAILED".to_string()
        };

        let mut changed_files = checkpoint.modified_files.clone();
        changed_files.extend(checkpoint.added_files.clone());
        changed_files.extend(checkpoint.untracked_files.clone());

        let mut next_actions = Vec::new();
        if overall_success {
            next_actions.push("Review unified diff and verification results in report.md".into());
            next_actions
                .push("Explicitly apply or commit the changes to your target branch".into());
        } else {
            if !worker_ok {
                next_actions.push(format!(
                    "Inspect worker failure diagnostics: {}",
                    worker_err_msg.unwrap_or_default()
                ));
            }
            if !all_required_passed {
                next_actions.push("Fix failing verification checks reported in report.md".into());
            }
        }

        let report = EvidenceReport {
            run_id: run_id.clone(),
            feature_id,
            base_commit,
            contract_hash: spec_hash,
            status: final_status.clone(),
            started_at,
            completed_at,
            duration_seconds,
            changed_files,
            unified_diff: checkpoint.unified_diff,
            checks: check_results,
            all_required_checks_passed: all_required_passed,
            telemetry: TelemetryReport {
                observed_usage,
                estimated_cost_usd: estimated_cost,
                billed_cost_usd: None,
                pricing_table_version: "2026-10-pricing-v1".into(),
                usage_source: if options.adapter_name == "claude" {
                    "claude stream-json telemetry".into()
                } else {
                    "mock telemetry".into()
                },
            },
            next_actions,
        };

        // Save report artifacts
        report.save_to_dir(store.artifacts_dir())?;
        report.save_to_dir(store.run_dir())?;

        // 9. Final State Transition
        if overall_success {
            store.transition_to(
                RunState::ReviewReady,
                vec!["report.json".into(), "report.md".into()],
                TransitionOutcome::Success,
                Some("Single-agent run completed successfully and ready for review".into()),
            )?;
        } else {
            store.transition_to(
                RunState::Failed,
                vec!["report.json".into(), "report.md".into()],
                TransitionOutcome::Failure(
                    "One or more required checks or worker actions failed".into(),
                ),
                Some("Execution did not pass all required gates".into()),
            )?;
        }

        Ok(report)
    }
}
