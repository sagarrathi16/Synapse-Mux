use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use chrono::Utc;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::mpsc;
use tracing::{info, warn};
use uuid::Uuid;

use crate::adapters::claude::ClaudeCodeAdapter;
use crate::adapters::codex::CodexAdapter;
use crate::adapters::mock::MockAdapter;
use crate::adapters::{AgentEvent, ObservedUsage, WorkerAdapter, WorkerRequest};
use crate::check::{CheckRunner, CheckStatus};
use crate::git::{GitError, GitManager, MergeResult};
use crate::manifest::{ManifestError, SynapseSpec};
use crate::recovery::{CleanupSummary, RecoveryManager};
use crate::repair::ContextFirewall;
use crate::report::{EvidenceReport, TelemetryReport};
use crate::review::{ReviewReport, Reviewer};
use crate::state::{RunState, RunStore, StateError, TransitionOutcome};
use crate::verifier::VerificationPipeline;

#[derive(Error, Debug)]
pub enum RunnerError {
    #[error("Git error: {0}")]
    Git(#[from] GitError),
    #[error("State machine error: {0}")]
    State(#[from] StateError),
    #[error("Manifest / spec error: {0}")]
    Manifest(#[from] ManifestError),
    #[error("Adapter error: {0}")]
    Adapter(#[from] crate::adapters::AdapterError),
    #[error("Check error: {0}")]
    Check(#[from] crate::check::CheckError),
    #[error("Review error: {0}")]
    Review(#[from] crate::review::ReviewError),
    #[error("Security error: {0}")]
    Security(#[from] crate::security::SecurityError),
    #[error("Serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Contract hash mismatch: expected {expected}, computed {computed}")]
    ContractHashMismatch { expected: String, computed: String },
    #[error("Partition ownership violation detected:\n{0}")]
    PartitionViolation(String),
    #[error("Merge conflict in staging worktree:\n{0}")]
    MergeConflict(String),
    #[error("Run execution failed: {0}")]
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

#[derive(Clone)]
pub struct DualWorkerOptions {
    pub spec_path: PathBuf,
    pub run_id: Option<String>,
    pub allow_dirty_with_snapshot: bool,
    pub claude_adapter_name: String,
    pub codex_adapter_name: String,
    pub claude_prompt: Option<String>,
    pub codex_prompt: Option<String>,
    pub claude_mock_fixture: Option<PathBuf>,
    pub codex_mock_fixture: Option<PathBuf>,
    pub claude_mock_file_writes: Vec<(String, String)>,
    pub codex_mock_file_writes: Vec<(String, String)>,
    pub approve_hash: Option<String>,
    pub reviewer: Option<Arc<dyn Reviewer>>,
    pub repair_mock_file_writes: Vec<(String, String)>,
    pub max_repair_iterations: Option<usize>,
}

impl std::fmt::Debug for DualWorkerOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DualWorkerOptions")
            .field("spec_path", &self.spec_path)
            .field("run_id", &self.run_id)
            .field("allow_dirty_with_snapshot", &self.allow_dirty_with_snapshot)
            .field("claude_adapter_name", &self.claude_adapter_name)
            .field("codex_adapter_name", &self.codex_adapter_name)
            .field("has_reviewer", &self.reviewer.is_some())
            .field(
                "repair_mock_file_writes_count",
                &self.repair_mock_file_writes.len(),
            )
            .field("max_repair_iterations", &self.max_repair_iterations)
            .finish()
    }
}

impl Default for DualWorkerOptions {
    fn default() -> Self {
        Self {
            spec_path: PathBuf::from("synapse-spec.json"),
            run_id: None,
            allow_dirty_with_snapshot: false,
            claude_adapter_name: "claude".into(),
            codex_adapter_name: "codex".into(),
            claude_prompt: None,
            codex_prompt: None,
            claude_mock_fixture: None,
            codex_mock_fixture: None,
            claude_mock_file_writes: Vec::new(),
            codex_mock_file_writes: Vec::new(),
            approve_hash: None,
            reviewer: None,
            repair_mock_file_writes: Vec::new(),
            max_repair_iterations: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PartitionViolation {
    pub worker: String,
    pub file_path: String,
    pub violation_type: String,
    pub diff: String,
    pub archived_diff_file: String,
}

pub struct Orchestrator;

impl Orchestrator {
    pub async fn approve_contract(
        repo_root: impl AsRef<Path>,
        spec_path: impl AsRef<Path>,
        run_id: Option<String>,
        approve_hash: Option<String>,
    ) -> Result<String, RunnerError> {
        let repo_root = repo_root.as_ref();
        let spec_path = spec_path.as_ref();

        let spec = SynapseSpec::from_file(spec_path)?;
        spec.validate(repo_root)?;
        let computed_hash = spec.compute_contract_hash(repo_root)?;

        if let Some(ref expected) = approve_hash
            && expected != &computed_hash
        {
            return Err(RunnerError::ContractHashMismatch {
                expected: expected.clone(),
                computed: computed_hash,
            });
        }

        let run_id = run_id.unwrap_or(spec.run_id);
        let store = RunStore::new(repo_root, &run_id)?;

        // Ensure run state exists and transition to ContractApproval
        if store.load_state_snapshot().is_err() {
            store.initialize(Some(spec.base_commit), Some(computed_hash.clone()))?;
        }

        let _ = store.transition_to(
            RunState::ContractProposal,
            vec![],
            TransitionOutcome::Success,
            Some("Contract proposed".into()),
        );

        let approval_record = serde_json::json!({
            "run_id": run_id,
            "contract_hash": computed_hash,
            "approved_at": Utc::now().to_rfc3339(),
            "contract_files": spec.contract_files,
            "contract_tests": spec.contract_tests,
        });

        store.save_artifact("approval.json", approval_record.to_string().as_bytes())?;

        store.transition_to(
            RunState::ContractApproval,
            vec!["approval.json".into()],
            TransitionOutcome::Success,
            Some(format!(
                "Contract hash {computed_hash} approved by engineer"
            )),
        )?;

        info!(
            "Contract approved for run '{}' (hash: {})",
            run_id, computed_hash
        );
        Ok(computed_hash)
    }

    pub async fn run_single_agent(
        repo_root: impl AsRef<Path>,
        options: SingleWorkerOptions,
    ) -> Result<EvidenceReport, RunnerError> {
        let start_time = Instant::now();
        let started_at = Utc::now();
        let repo_root = repo_root.as_ref();

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

        let store = RunStore::new(repo_root, &run_id)?;
        let git_mgr = GitManager::new(repo_root)?;

        info!("Starting single-agent run '{}'", run_id);

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
            "codex" => Arc::new(CodexAdapter::default()),
            _ => Arc::new(ClaudeCodeAdapter::default()),
        };

        let prompt = options
            .prompt
            .unwrap_or_else(|| "Implement the requested contract and changes.".into());

        let timeout_seconds = spec
            .as_ref()
            .map(|s| s.limits.worker_timeout_seconds)
            .unwrap_or(600);

        let mut env_vars = HashMap::new();
        env_vars.insert("SYNAPSE_PORT_BASE".into(), "8080".into());
        env_vars.insert(
            "SYNAPSE_DB_SCHEMA".into(),
            format!("synapse_{}_single", run_id.replace('-', "_")),
        );

        let worker_req = WorkerRequest {
            run_id: run_id.clone(),
            worker_name: "worker-single".into(),
            prompt,
            worktree_dir: worktree_dir.clone(),
            timeout_seconds,
            max_turns: Some(30),
            max_budget_usd: Some(5.0),
            model: Some("claude-3-7-sonnet".into()),
            env_vars,
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

        if let Err(e) = git_mgr.remove_worktree(&worktree_dir).await {
            warn!(
                "Failed to cleanly remove worktree at {:?}: {}",
                worktree_dir, e
            );
        }

        let completed_at = Utc::now();
        let duration_seconds = start_time.elapsed().as_secs_f64();

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
            review_findings: Vec::new(),
            executed_adversarial_checks: Vec::new(),
            repair_iterations_count: 0,
            telemetry: TelemetryReport {
                observed_usage,
                estimated_cost_usd: estimated_cost,
                billed_cost_usd: None,
                pricing_table_version: "2026-10-pricing-v1".into(),
                usage_source: if options.adapter_name == "claude" {
                    "claude stream-json telemetry".into()
                } else if options.adapter_name == "codex" {
                    "codex json telemetry".into()
                } else {
                    "mock telemetry".into()
                },
            },
            next_actions,
        };

        report.save_to_dir(store.artifacts_dir())?;
        report.save_to_dir(store.run_dir())?;

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

    pub async fn run_dual_agent(
        repo_root: impl AsRef<Path>,
        options: DualWorkerOptions,
    ) -> Result<EvidenceReport, RunnerError> {
        let start_time = Instant::now();
        let started_at = Utc::now();
        let repo_root = repo_root.as_ref();

        // 1. Validate spec & compute hash
        let spec = SynapseSpec::from_file(&options.spec_path)?;
        spec.validate(repo_root)?;
        let computed_hash = spec.compute_contract_hash(repo_root)?;

        if let Some(ref expected) = options.approve_hash
            && expected != &computed_hash
        {
            return Err(RunnerError::ContractHashMismatch {
                expected: expected.clone(),
                computed: computed_hash,
            });
        }

        let run_id = options.run_id.unwrap_or_else(|| spec.run_id.clone());
        let feature_id = spec.feature_id.clone();
        let base_commit = spec.base_commit.clone();

        let store = RunStore::new(repo_root, &run_id)?;
        let git_mgr = GitManager::new(repo_root)?;

        info!(
            "Starting dual-agent orchestration run '{}' for feature '{}'",
            run_id, feature_id
        );

        // 2. Preflight Check
        let preflight = match git_mgr.preflight(options.allow_dirty_with_snapshot).await {
            Ok(p) => p,
            Err(e) => {
                store.initialize(Some(base_commit.clone()), Some(computed_hash.clone()))?;
                store.transition_to(
                    RunState::Failed,
                    vec![],
                    TransitionOutcome::Failure(format!("Preflight check failed: {e}")),
                    Some("Preflight rejected workspace state".into()),
                )?;
                return Err(RunnerError::Git(e));
            }
        };

        store.initialize(Some(base_commit.clone()), Some(computed_hash.clone()))?;

        let preflight_json = serde_json::to_string_pretty(&preflight)?;
        store.save_artifact("preflight.json", preflight_json.as_bytes())?;

        // 3. State: ContractApproval -> Implementation
        store.transition_to(
            RunState::ContractApproval,
            vec!["preflight.json".into()],
            TransitionOutcome::Success,
            Some(format!(
                "Contract hash {computed_hash} validated and approved"
            )),
        )?;

        store.transition_to(
            RunState::Implementation,
            vec![],
            TransitionOutcome::Success,
            Some("Launching parallel workers WTA (Claude) and WTB (Codex)".into()),
        )?;

        // 4. Provision WTA (Claude) and WTB (Codex)
        let wta_dir = match git_mgr
            .create_worktree(&run_id, "claude", &base_commit)
            .await
        {
            Ok(d) => d,
            Err(e) => {
                store.transition_to(
                    RunState::Failed,
                    vec![],
                    TransitionOutcome::Failure(format!("Failed to create Claude worktree: {e}")),
                    None,
                )?;
                return Err(RunnerError::Git(e));
            }
        };

        let wtb_dir = match git_mgr
            .create_worktree(&run_id, "codex", &base_commit)
            .await
        {
            Ok(d) => d,
            Err(e) => {
                let _ = git_mgr.remove_worktree(&wta_dir).await;
                store.transition_to(
                    RunState::Failed,
                    vec![],
                    TransitionOutcome::Failure(format!("Failed to create Codex worktree: {e}")),
                    None,
                )?;
                return Err(RunnerError::Git(e));
            }
        };

        // Apply read-only guardrails on contract files in both worktrees
        let mut locked_files = spec.contract_files.clone();
        locked_files.extend(spec.contract_tests.clone());
        let _ = GitManager::set_files_readonly(&wta_dir, &locked_files);
        let _ = GitManager::set_files_readonly(&wtb_dir, &locked_files);

        // Setup logging channels
        let events_claude_path = store.artifacts_dir().join("events_claude.jsonl");
        let events_codex_path = store.artifacts_dir().join("events_codex.jsonl");
        let (tx_claude, mut rx_claude) = mpsc::channel::<AgentEvent>(100);
        let (tx_codex, mut rx_codex) = mpsc::channel::<AgentEvent>(100);

        let logger_claude = tokio::spawn(async move {
            let mut file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&events_claude_path)
                .ok();
            while let Some(event) = rx_claude.recv().await {
                if let Some(ref mut f) = file
                    && let Ok(json_line) = serde_json::to_string(&event)
                {
                    let _ = writeln!(f, "{json_line}");
                }
            }
        });

        let logger_codex = tokio::spawn(async move {
            let mut file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&events_codex_path)
                .ok();
            while let Some(event) = rx_codex.recv().await {
                if let Some(ref mut f) = file
                    && let Ok(json_line) = serde_json::to_string(&event)
                {
                    let _ = writeln!(f, "{json_line}");
                }
            }
        });

        // Adapters
        let adapter_claude: Arc<dyn WorkerAdapter> = match options.claude_adapter_name.as_str() {
            "mock" => {
                let mut mock = MockAdapter::new();
                if let Some(ref f) = options.claude_mock_fixture {
                    mock = mock.with_fixture(f);
                }
                for (p, c) in options.claude_mock_file_writes {
                    mock = mock.with_file_write(p, c);
                }
                Arc::new(mock)
            }
            _ => Arc::new(ClaudeCodeAdapter::default()),
        };

        let adapter_codex: Arc<dyn WorkerAdapter> = match options.codex_adapter_name.as_str() {
            "mock" => {
                let mut mock = MockAdapter::new();
                if let Some(ref f) = options.codex_mock_fixture {
                    mock = mock.with_fixture(f);
                }
                for (p, c) in options.codex_mock_file_writes {
                    mock = mock.with_file_write(p, c);
                }
                Arc::new(mock)
            }
            _ => Arc::new(CodexAdapter::default()),
        };

        // Environment isolation
        let clean_run_id = run_id.replace('-', "_");
        let mut env_claude = HashMap::new();
        env_claude.insert("SYNAPSE_PORT_BASE".into(), "8100".into());
        env_claude.insert(
            "SYNAPSE_DB_SCHEMA".into(),
            format!("synapse_{}_claude", clean_run_id),
        );

        let mut env_codex = HashMap::new();
        env_codex.insert("SYNAPSE_PORT_BASE".into(), "8200".into());
        env_codex.insert(
            "SYNAPSE_DB_SCHEMA".into(),
            format!("synapse_{}_codex", clean_run_id),
        );

        let timeout = spec.limits.worker_timeout_seconds;
        let prompt_claude = options
            .claude_prompt
            .unwrap_or_else(|| "Implement API layer and assigned partition tasks.".into());
        let prompt_codex = options.codex_prompt.unwrap_or_else(|| {
            "Implement worker/repository layer and assigned partition tasks.".into()
        });

        let req_claude = WorkerRequest {
            run_id: run_id.clone(),
            worker_name: "claude".into(),
            prompt: prompt_claude,
            worktree_dir: wta_dir.clone(),
            timeout_seconds: timeout,
            max_turns: Some(30),
            max_budget_usd: Some(5.0),
            model: Some("claude-3-7-sonnet".into()),
            env_vars: env_claude.clone(),
        };

        let req_codex = WorkerRequest {
            run_id: run_id.clone(),
            worker_name: "codex".into(),
            prompt: prompt_codex,
            worktree_dir: wtb_dir.clone(),
            timeout_seconds: timeout,
            max_turns: Some(30),
            max_budget_usd: Some(5.0),
            model: Some("gpt-4o".into()),
            env_vars: env_codex.clone(),
        };

        // 5. Run workers concurrently
        let (res_claude, res_codex) = tokio::join!(
            adapter_claude.run_worker(req_claude, tx_claude),
            adapter_codex.run_worker(req_codex, tx_codex)
        );

        let _ = logger_claude.await;
        let _ = logger_codex.await;

        let (claude_ok, claude_usage, claude_cost) = match res_claude {
            Ok(r) => (r.success, r.observed_usage, r.estimated_cost_usd),
            Err(_e) => (false, ObservedUsage::default(), 0.0),
        };

        let (codex_ok, codex_usage, codex_cost) = match res_codex {
            Ok(r) => (r.success, r.observed_usage, r.estimated_cost_usd),
            Err(_e) => (false, ObservedUsage::default(), 0.0),
        };

        // 6. Checkpoint state
        store.transition_to(
            RunState::Checkpoint,
            vec!["events_claude.jsonl".into(), "events_codex.jsonl".into()],
            TransitionOutcome::Success,
            Some(
                "Workers completed, capturing checkpoints and validating partition ownership"
                    .into(),
            ),
        )?;

        let cp_claude = git_mgr
            .capture_checkpoint(
                &wta_dir,
                &run_id,
                "claude",
                &base_commit,
                Some(&computed_hash),
            )
            .await?;
        let cp_codex = git_mgr
            .capture_checkpoint(
                &wtb_dir,
                &run_id,
                "codex",
                &base_commit,
                Some(&computed_hash),
            )
            .await?;

        store.save_artifact(
            "checkpoint_claude.json",
            serde_json::to_string_pretty(&cp_claude)?.as_bytes(),
        )?;
        store.save_artifact(
            "checkpoint_codex.json",
            serde_json::to_string_pretty(&cp_codex)?.as_bytes(),
        )?;

        // 7. Ownership & Contract Enforcement
        let mut violations = Vec::new();
        let violations_dir = store.run_dir().join("violations");
        fs::create_dir_all(&violations_dir)?;

        for (worker_name, cp, worktree_dir) in [
            ("claude", &cp_claude, &wta_dir),
            ("codex", &cp_codex, &wtb_dir),
        ] {
            let mut all_files = cp.modified_files.clone();
            all_files.extend(cp.added_files.clone());
            all_files.extend(cp.deleted_files.clone());
            all_files.extend(cp.untracked_files.clone());

            let partition_globs = spec
                .partitions
                .get(worker_name)
                .map(|p| &p.owned_globs)
                .cloned()
                .unwrap_or_default();

            for f in all_files {
                let f = f.replace('\\', "/");
                let is_contract =
                    spec.contract_files.contains(&f) || spec.contract_tests.contains(&f);
                if is_contract {
                    let diff = git_mgr
                        .run_git(&["diff", "HEAD", "--", &f], Some(worktree_dir))
                        .await
                        .unwrap_or_default();
                    let slug = f.replace(['/', '\\'], "_");
                    let v_file = format!("{worker_name}_{slug}.diff");
                    violations.push(PartitionViolation {
                        worker: worker_name.to_string(),
                        file_path: f.clone(),
                        violation_type: "contract_mutation".into(),
                        diff,
                        archived_diff_file: v_file,
                    });
                    continue;
                }

                // Check partition ownership
                let owned = partition_globs
                    .iter()
                    .any(|g| SynapseSpec::path_matches_glob(&f, g));
                let shared_owned = spec
                    .shared_file_policy
                    .get(&f)
                    .map(|rule| rule.owner == worker_name)
                    .unwrap_or(false);

                if !owned && !shared_owned {
                    let diff = git_mgr
                        .run_git(&["diff", "HEAD", "--", &f], Some(worktree_dir))
                        .await
                        .unwrap_or_default();
                    let slug = f.replace(['/', '\\'], "_");
                    let v_file = format!("{worker_name}_{slug}.diff");
                    violations.push(PartitionViolation {
                        worker: worker_name.to_string(),
                        file_path: f.clone(),
                        violation_type: "out_of_partition".into(),
                        diff,
                        archived_diff_file: v_file,
                    });
                }
            }
        }

        if !violations.is_empty() {
            // Archive violation diffs
            for v in &violations {
                let v_path = violations_dir.join(&v.archived_diff_file);
                let _ = fs::write(&v_path, &v.diff);
            }

            let v_json = serde_json::to_string_pretty(&violations)?;
            store.save_artifact("violations.json", v_json.as_bytes())?;

            let _ = git_mgr.remove_worktree(&wta_dir).await;
            let _ = git_mgr.remove_worktree(&wtb_dir).await;

            store.transition_to(
                RunState::Failed,
                vec!["violations.json".into()],
                TransitionOutcome::Failure(format!(
                    "Found {} partition ownership or contract violations",
                    violations.len()
                )),
                Some("Halted integration due to partition boundary violation".into()),
            )?;

            let msg = violations
                .iter()
                .map(|v| {
                    format!(
                        "Worker '{}' {} in '{}'",
                        v.worker, v.violation_type, v.file_path
                    )
                })
                .collect::<Vec<_>>()
                .join("\n");

            return Err(RunnerError::PartitionViolation(msg));
        }

        // 8. State: INTEGRATION
        store.transition_to(
            RunState::Integration,
            vec![
                "checkpoint_claude.json".into(),
                "checkpoint_codex.json".into(),
            ],
            TransitionOutcome::Success,
            Some("Partition checks clean; proceeding to staging integration".into()),
        )?;

        // Commit checkpoints to internal refs
        let sha_claude = git_mgr
            .commit_worktree(&wta_dir, "synapse: checkpoint claude")
            .await?;
        let ref_claude = format!("refs/synapse/{}/checkpoint-claude", run_id);
        git_mgr.update_ref(&ref_claude, &sha_claude).await?;

        let sha_codex = git_mgr
            .commit_worktree(&wtb_dir, "synapse: checkpoint codex")
            .await?;
        let ref_codex = format!("refs/synapse/{}/checkpoint-codex", run_id);
        git_mgr.update_ref(&ref_codex, &sha_codex).await?;

        // Provision staging worktree
        let wt_stage = match git_mgr
            .create_worktree(&run_id, "stage", &base_commit)
            .await
        {
            Ok(d) => d,
            Err(e) => {
                let _ = git_mgr.remove_worktree(&wta_dir).await;
                let _ = git_mgr.remove_worktree(&wtb_dir).await;
                store.transition_to(
                    RunState::Failed,
                    vec![],
                    TransitionOutcome::Failure(format!("Failed to create staging worktree: {e}")),
                    None,
                )?;
                return Err(RunnerError::Git(e));
            }
        };

        // Merge Claude checkpoint
        let merge_claude = git_mgr
            .merge_ref(
                &wt_stage,
                &ref_claude,
                "synapse: integrate checkpoint claude",
            )
            .await?;
        if let MergeResult::Conflict {
            conflicting_files,
            conflict_diff,
        } = merge_claude
        {
            let conflict_json = serde_json::json!({
                "worker": "claude",
                "conflicts": conflicting_files,
            });
            store.save_artifact("conflicts.json", conflict_json.to_string().as_bytes())?;
            store.save_artifact("conflicts.diff", conflict_diff.as_bytes())?;

            let _ = git_mgr.remove_worktree(&wta_dir).await;
            let _ = git_mgr.remove_worktree(&wtb_dir).await;
            let _ = git_mgr.remove_worktree(&wt_stage).await;

            store.transition_to(
                RunState::NeedsUser,
                vec!["conflicts.json".into(), "conflicts.diff".into()],
                TransitionOutcome::UserActionRequired(
                    "Merge conflict applying Claude checkpoint".into(),
                ),
                Some("Halted due to merge conflicts in staging".into()),
            )?;

            return Err(RunnerError::MergeConflict(conflicting_files.join(", ")));
        }

        // Merge Codex checkpoint
        let merge_codex = git_mgr
            .merge_ref(&wt_stage, &ref_codex, "synapse: integrate checkpoint codex")
            .await?;
        if let MergeResult::Conflict {
            conflicting_files,
            conflict_diff,
        } = merge_codex
        {
            let conflict_json = serde_json::json!({
                "worker": "codex",
                "conflicts": conflicting_files,
            });
            store.save_artifact("conflicts.json", conflict_json.to_string().as_bytes())?;
            store.save_artifact("conflicts.diff", conflict_diff.as_bytes())?;

            let _ = git_mgr.remove_worktree(&wta_dir).await;
            let _ = git_mgr.remove_worktree(&wtb_dir).await;
            let _ = git_mgr.remove_worktree(&wt_stage).await;

            store.transition_to(
                RunState::NeedsUser,
                vec!["conflicts.json".into(), "conflicts.diff".into()],
                TransitionOutcome::UserActionRequired(
                    "Merge conflict applying Codex checkpoint".into(),
                ),
                Some("Halted due to merge conflicts in staging".into()),
            )?;

            return Err(RunnerError::MergeConflict(conflicting_files.join(", ")));
        }

        // Handle lockfile regeneration
        for (file, rule) in &spec.shared_file_policy {
            if rule.resolution.as_deref() == Some("regenerate") {
                let _ = git_mgr.checkout_file(&wt_stage, "HEAD", file).await;
            }
        }

        // 9. State: VERIFICATION
        store.transition_to(
            RunState::Verification,
            vec![],
            TransitionOutcome::Success,
            Some("Running deterministic verification suite against staging worktree".into()),
        )?;

        let mut verification_report =
            VerificationPipeline::verify_staging(&wt_stage, repo_root, &spec, Some(&computed_hash))
                .await?;

        // Independent Model Review & proposed adversarial tests (PRD Section 6 FR-6)
        let mut review_report: Option<ReviewReport> = None;
        let mut adversarial_results = Vec::new();
        if let Some(ref rev) = options.reviewer {
            let staging_diff = git_mgr
                .run_git(&["diff", &base_commit], Some(&wt_stage))
                .await
                .unwrap_or_default();

            // Capture individual worker partition diffs for cross-model review
            // (Codex reviews Claude's partition; Claude reviews Codex's partition)
            let claude_diff = git_mgr
                .run_git(&["diff", &base_commit], Some(&wta_dir))
                .await
                .unwrap_or_default();
            let codex_diff = git_mgr
                .run_git(&["diff", &base_commit], Some(&wtb_dir))
                .await
                .unwrap_or_default();

            let _ = crate::context_packer::ContextPacker::pack_cross_review_payload(
                &run_id,
                "codex",
                "openai",
                "claude",
                &claude_diff,
                &spec,
                &computed_hash,
                spec.limits.context_bundle_max_bytes,
                store.run_dir(),
            );
            let _ = crate::context_packer::ContextPacker::pack_cross_review_payload(
                &run_id,
                "claude",
                "anthropic",
                "codex",
                &codex_diff,
                &spec,
                &computed_hash,
                spec.limits.context_bundle_max_bytes,
                store.run_dir(),
            );

            let res = rev
                .perform_review(&staging_diff, &spec.contract_files, &spec, &wt_stage)
                .await?;
            adversarial_results = res.executed_adversarial_results.clone();
            review_report = Some(res);
        }

        let mut all_adversarial_passed = adversarial_results
            .iter()
            .filter(|c| c.required)
            .all(|c| c.status == CheckStatus::Passed);

        store.save_artifact(
            "checks.json",
            serde_json::to_string_pretty(&verification_report.check_results)?.as_bytes(),
        )?;
        if let Some(ref rev) = review_report {
            store.save_artifact("review.json", serde_json::to_string_pretty(rev)?.as_bytes())?;
        }

        // 10. Bounded Repair Loop (PRD Section 6 FR-7)
        let mut repair_iteration = 0;
        let max_repair_iters = options
            .max_repair_iterations
            .unwrap_or(spec.limits.max_repair_iterations);

        let mut current_wt_stage = wt_stage;

        while (!verification_report.all_required_passed || !all_adversarial_passed)
            && repair_iteration < max_repair_iters
        {
            repair_iteration += 1;
            info!(
                "Entering Repair Loop (iteration {} of {})",
                repair_iteration, max_repair_iters
            );

            let responsible_workers = ContextFirewall::determine_responsible_workers(
                &verification_report.failed_checks,
                &spec,
            );

            store.transition_to(
                RunState::Repair,
                vec!["checks.json".into()],
                TransitionOutcome::Success,
                Some(format!(
                    "Repair iteration {} of {} targeting: {}",
                    repair_iteration,
                    max_repair_iters,
                    responsible_workers.join(", ")
                )),
            )?;

            for worker_name in &responsible_workers {
                let (worker_dir, adapter) = if worker_name == "claude" {
                    (&wta_dir, &adapter_claude)
                } else {
                    (&wtb_dir, &adapter_codex)
                };

                let worker_diff = git_mgr
                    .run_git(&["diff", &base_commit], Some(worker_dir))
                    .await
                    .unwrap_or_default();

                let findings = review_report
                    .as_ref()
                    .map(|r| r.findings.as_slice())
                    .unwrap_or(&[]);

                let repair_bundle = crate::context_packer::ContextPacker::pack_repair_bundle(
                    &run_id,
                    repair_iteration,
                    max_repair_iters,
                    worker_name,
                    if worker_name == "claude" {
                        "anthropic"
                    } else {
                        "openai"
                    },
                    &computed_hash,
                    &spec,
                    &worker_diff,
                    &verification_report.failed_checks,
                    findings,
                    spec.limits.context_bundle_max_bytes,
                    store.run_dir(),
                )?;

                let bundle_filename = format!(
                    "repair_context_iter_{}_{}.json",
                    repair_iteration, worker_name
                );
                store.save_artifact(
                    &bundle_filename,
                    serde_json::to_string_pretty(&repair_bundle)?.as_bytes(),
                )?;

                let repair_prompt = repair_bundle.format_prompt();

                let (tx, mut rx) = mpsc::channel(50);
                let log_sink = tokio::spawn(async move { while rx.recv().await.is_some() {} });

                let repair_req = WorkerRequest {
                    run_id: run_id.clone(),
                    worker_name: worker_name.clone(),
                    prompt: repair_prompt,
                    worktree_dir: worker_dir.clone(),
                    timeout_seconds: spec.limits.worker_timeout_seconds,
                    max_turns: Some(15),
                    max_budget_usd: Some(2.0),
                    model: None,
                    env_vars: if worker_name == "claude" {
                        env_claude.clone()
                    } else {
                        env_codex.clone()
                    },
                };

                let _ = adapter.run_worker(repair_req, tx).await;
                let _ = log_sink.await;

                // Apply any test repair writes if configured in options
                for (rel_path, content) in &options.repair_mock_file_writes {
                    let target = worker_dir.join(rel_path);
                    if let Some(p) = target.parent() {
                        let _ = fs::create_dir_all(p);
                    }
                    let _ = fs::write(target, content);
                }

                // Commit repair in worker worktree and update checkpoint ref
                let sha = git_mgr
                    .commit_worktree(
                        worker_dir,
                        &format!("synapse: repair iter {} {}", repair_iteration, worker_name),
                    )
                    .await?;
                let ref_name = format!("refs/synapse/{}/checkpoint-{}", run_id, worker_name);
                git_mgr.update_ref(&ref_name, &sha).await?;
            }

            // Re-provision WT_STAGE and re-merge checkpoints
            let _ = git_mgr.remove_worktree(&current_wt_stage).await;
            current_wt_stage = git_mgr
                .create_worktree(&run_id, "stage", &base_commit)
                .await?;

            let _ = git_mgr
                .merge_ref(
                    &current_wt_stage,
                    &ref_claude,
                    "synapse: integrate repair claude",
                )
                .await?;
            let _ = git_mgr
                .merge_ref(
                    &current_wt_stage,
                    &ref_codex,
                    "synapse: integrate repair codex",
                )
                .await?;

            for (file, rule) in &spec.shared_file_policy {
                if rule.resolution.as_deref() == Some("regenerate") {
                    let _ = git_mgr.checkout_file(&current_wt_stage, "HEAD", file).await;
                }
            }

            // Re-verify staging
            let _ = store.transition_to(
                RunState::Verification,
                vec![],
                TransitionOutcome::Success,
                Some(format!(
                    "Re-verifying staging worktree after repair iteration {}",
                    repair_iteration
                )),
            );

            verification_report = VerificationPipeline::verify_staging(
                &current_wt_stage,
                repo_root,
                &spec,
                Some(&computed_hash),
            )
            .await?;

            if let Some(ref rev) = review_report {
                let executed = crate::review::execute_adversarial_tests(
                    &current_wt_stage,
                    &rev.proposed_tests,
                )
                .await?;
                all_adversarial_passed = executed
                    .iter()
                    .filter(|c| c.required)
                    .all(|c| c.status == CheckStatus::Passed);
                adversarial_results = executed;
            }
        }

        // Capture unified staging diff against base_commit
        let staging_diff = git_mgr
            .run_git(&["diff", &base_commit], Some(&current_wt_stage))
            .await
            .unwrap_or_default();

        let diff_names_out = git_mgr
            .run_git(
                &["diff", "--name-only", &base_commit],
                Some(&current_wt_stage),
            )
            .await
            .unwrap_or_default();

        let mut changed_files = Vec::new();
        for line in diff_names_out.lines() {
            let trimmed = line.trim();
            if !trimmed.is_empty() {
                changed_files.push(trimmed.replace('\\', "/"));
            }
        }

        // Clean up worktrees
        let _ = git_mgr.remove_worktree(&wta_dir).await;
        let _ = git_mgr.remove_worktree(&wtb_dir).await;
        let _ = git_mgr.remove_worktree(&current_wt_stage).await;

        let completed_at = Utc::now();
        let duration_seconds = start_time.elapsed().as_secs_f64();
        let overall_success = claude_ok
            && codex_ok
            && verification_report.all_required_passed
            && all_adversarial_passed;

        let total_observed_usage = ObservedUsage {
            input_tokens: claude_usage.input_tokens + codex_usage.input_tokens,
            output_tokens: claude_usage.output_tokens + codex_usage.output_tokens,
            cache_read_input_tokens: Some(
                claude_usage.cache_read_input_tokens.unwrap_or(0)
                    + codex_usage.cache_read_input_tokens.unwrap_or(0),
            ),
            cache_creation_input_tokens: None,
        };

        let report = EvidenceReport {
            run_id: run_id.clone(),
            feature_id,
            base_commit,
            contract_hash: Some(computed_hash),
            status: if overall_success {
                "REVIEW_READY".into()
            } else {
                "FAILED".into()
            },
            started_at,
            completed_at,
            duration_seconds,
            changed_files,
            unified_diff: staging_diff,
            checks: verification_report.check_results,
            all_required_checks_passed: verification_report.all_required_passed
                && all_adversarial_passed,
            review_findings: review_report
                .as_ref()
                .map(|r| r.findings.clone())
                .unwrap_or_default(),
            executed_adversarial_checks: adversarial_results,
            repair_iterations_count: repair_iteration,
            telemetry: TelemetryReport {
                observed_usage: total_observed_usage,
                estimated_cost_usd: claude_cost + codex_cost,
                billed_cost_usd: None,
                pricing_table_version: "2026-10-pricing-v1".into(),
                usage_source: "heterogeneous claude+codex streams".into(),
            },
            next_actions: if overall_success {
                vec![
                    "Review merged unified diff in report.md".into(),
                    "Explicitly checkout and apply staging changes to target branch".into(),
                ]
            } else {
                vec![
                    "Inspect failed verification checks in report.md".into(),
                    "Trigger bounded repair loop on responsible partition or resolve manually"
                        .into(),
                ]
            },
        };

        report.save_to_dir(store.artifacts_dir())?;
        report.save_to_dir(store.run_dir())?;

        if overall_success {
            store.transition_to(
                RunState::ReviewReady,
                vec!["report.json".into(), "report.md".into()],
                TransitionOutcome::Success,
                Some("Dual-agent run integrated and verified successfully".into()),
            )?;
        } else {
            let (terminal_state, reason) = if repair_iteration >= max_repair_iters {
                (
                    RunState::NeedsUser,
                    format!(
                        "MaxRepairsExceeded: verification failed after {} repair iterations",
                        repair_iteration
                    ),
                )
            } else {
                (
                    RunState::Failed,
                    format!(
                        "Verification gate failed: {}",
                        verification_report.diagnostics_summary
                    ),
                )
            };
            store.transition_to(
                terminal_state,
                vec!["report.json".into(), "report.md".into()],
                TransitionOutcome::Failure(reason),
                Some("Verification gate failed".into()),
            )?;
        }

        Ok(report)
    }

    pub async fn resume_dual_agent(
        repo_root: impl AsRef<Path>,
        options: DualWorkerOptions,
    ) -> Result<EvidenceReport, RunnerError> {
        let repo_root = repo_root.as_ref();
        let spec = SynapseSpec::from_file(&options.spec_path)?;
        let run_id = options
            .run_id
            .clone()
            .unwrap_or_else(|| spec.run_id.clone());

        let inspection = RecoveryManager::inspect_run(repo_root, &run_id).await?;
        if !inspection.can_resume {
            return Err(RunnerError::ExecutionFailed(format!(
                "Cannot resume run '{}': {}",
                run_id, inspection.reason
            )));
        }

        if inspection.reconciliation_required {
            return Err(RunnerError::ExecutionFailed(format!(
                "Repository HEAD has diverged from base commit {:?}. Manual reconciliation required.",
                inspection.base_commit
            )));
        }

        // If checkpoint refs already exist, perform idempotent merge and verification
        if inspection.claude_checkpoint_ref_exists && inspection.codex_checkpoint_ref_exists {
            info!("Resuming run '{}' from captured checkpoints", run_id);
            let git_mgr = GitManager::new(repo_root)?;
            let store = RunStore::new(repo_root, &run_id)?;
            let computed_hash = spec.compute_contract_hash(repo_root)?;
            let base_commit = spec.base_commit.clone();
            let feature_id = spec.feature_id.clone();

            let ref_claude = format!("refs/synapse/{run_id}/checkpoint-claude");
            let ref_codex = format!("refs/synapse/{run_id}/checkpoint-codex");

            let wt_stage = git_mgr
                .create_worktree(&run_id, "stage", &base_commit)
                .await?;
            git_mgr
                .merge_ref(&wt_stage, &ref_claude, "synapse: resume integrate claude")
                .await?;
            git_mgr
                .merge_ref(&wt_stage, &ref_codex, "synapse: resume integrate codex")
                .await?;

            for (file, rule) in &spec.shared_file_policy {
                if rule.resolution.as_deref() == Some("regenerate") {
                    let _ = git_mgr.checkout_file(&wt_stage, "HEAD", file).await;
                }
            }

            store.transition_to(
                RunState::Verification,
                vec![],
                TransitionOutcome::Success,
                Some("Resumed run: executing verification suite in staging".into()),
            )?;

            let verification_report = VerificationPipeline::verify_staging(
                &wt_stage,
                repo_root,
                &spec,
                Some(&computed_hash),
            )
            .await?;

            let staging_diff = git_mgr
                .run_git(&["diff", &base_commit], Some(&wt_stage))
                .await
                .unwrap_or_default();

            let diff_names_out = git_mgr
                .run_git(&["diff", "--name-only", &base_commit], Some(&wt_stage))
                .await
                .unwrap_or_default();

            let mut changed_files = Vec::new();
            for line in diff_names_out.lines() {
                let trimmed = line.trim();
                if !trimmed.is_empty() {
                    changed_files.push(trimmed.replace('\\', "/"));
                }
            }

            let _ = git_mgr.remove_worktree(&wt_stage).await;

            let report = EvidenceReport {
                run_id: run_id.clone(),
                feature_id,
                base_commit,
                contract_hash: Some(computed_hash),
                status: if verification_report.all_required_passed {
                    "REVIEW_READY".into()
                } else {
                    "FAILED".into()
                },
                started_at: Utc::now(),
                completed_at: Utc::now(),
                duration_seconds: 0.0,
                changed_files,
                unified_diff: staging_diff,
                checks: verification_report.check_results,
                all_required_checks_passed: verification_report.all_required_passed,
                review_findings: Vec::new(),
                executed_adversarial_checks: Vec::new(),
                repair_iterations_count: 0,
                telemetry: TelemetryReport {
                    observed_usage: ObservedUsage::default(),
                    estimated_cost_usd: 0.0,
                    billed_cost_usd: None,
                    pricing_table_version: "2026-10-pricing-v1".into(),
                    usage_source: "resumed checkpoints".into(),
                },
                next_actions: vec!["Review resumed report".into()],
            };

            report.save_to_dir(store.artifacts_dir())?;
            report.save_to_dir(store.run_dir())?;

            if verification_report.all_required_passed {
                store.transition_to(
                    RunState::ReviewReady,
                    vec!["report.json".into(), "report.md".into()],
                    TransitionOutcome::Success,
                    Some("Resumed run verified successfully".into()),
                )?;
            } else {
                store.transition_to(
                    RunState::NeedsUser,
                    vec!["report.json".into(), "report.md".into()],
                    TransitionOutcome::Failure("Resumed run verification checks failed".into()),
                    Some("Verification failed on resume".into()),
                )?;
            }

            Ok(report)
        } else {
            Self::run_dual_agent(repo_root, options).await
        }
    }

    pub async fn cancel_run(
        repo_root: impl AsRef<Path>,
        run_id: &str,
        reason: Option<String>,
    ) -> Result<(), RunnerError> {
        RecoveryManager::cancel_run(repo_root, run_id, reason).await
    }

    pub async fn cleanup_run(
        repo_root: impl AsRef<Path>,
        run_id: &str,
        keep_artifacts: bool,
    ) -> Result<CleanupSummary, RunnerError> {
        RecoveryManager::cleanup_run(repo_root, run_id, keep_artifacts).await
    }

    pub async fn export_diagnostic_bundle(
        repo_root: impl AsRef<Path>,
        run_id: &str,
        output_path: Option<PathBuf>,
    ) -> Result<PathBuf, RunnerError> {
        RecoveryManager::export_diagnostic_bundle(repo_root, run_id, output_path).await
    }
}
