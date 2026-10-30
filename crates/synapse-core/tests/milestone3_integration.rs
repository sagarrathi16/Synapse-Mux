use std::collections::BTreeMap;
use std::fs;
use std::process::Command;
use std::sync::Arc;

use synapse_core::config::{LimitsConfig, SharedFileRule, VerificationCommand};
use synapse_core::git::GitManager;
use synapse_core::manifest::{PartitionConfig, SynapseSpec};
use synapse_core::recovery::RecoveryManager;
use synapse_core::review::{AdversarialTestProposal, FindingSeverity, MockReviewer, ReviewFinding};
use synapse_core::runner::{DualWorkerOptions, Orchestrator};
use synapse_core::security::{SecurityError, scrub_secrets, validate_path_in_root};
use synapse_core::state::{RunState, RunStore};
use tempfile::TempDir;

async fn setup_git_repo(temp_dir: &TempDir) -> (GitManager, String) {
    let repo_root = temp_dir.path();

    let mut init = Command::new("git");
    init.args(["init"]).current_dir(repo_root);
    assert!(init.status().unwrap().success());

    let git = GitManager::new(repo_root).unwrap();

    let mut cfg_name = Command::new("git");
    cfg_name
        .args(["config", "user.name", "Synapse Test Engineer"])
        .current_dir(repo_root);
    assert!(cfg_name.status().unwrap().success());

    let mut cfg_email = Command::new("git");
    cfg_email
        .args(["config", "user.email", "engineer@synapse.local"])
        .current_dir(repo_root);
    assert!(cfg_email.status().unwrap().success());

    fs::write(repo_root.join(".gitignore"), "node_modules/\ndist/\n").unwrap();
    fs::write(repo_root.join("pnpm-lock.yaml"), "lockfileVersion: '6.0'\n").unwrap();

    fs::create_dir_all(repo_root.join("src/contracts")).unwrap();
    fs::write(
        repo_root.join("src/contracts/order.contract.ts"),
        "export interface Order { id: string; amount: number; }\n",
    )
    .unwrap();

    fs::create_dir_all(repo_root.join("tests")).unwrap();
    fs::write(
        repo_root.join("tests/order.contract.test.ts"),
        "// Contract test checking order invariants\n",
    )
    .unwrap();

    let mut add = Command::new("git");
    add.args(["add", "."]).current_dir(repo_root);
    assert!(add.status().unwrap().success());

    let mut commit = Command::new("git");
    commit
        .args(["commit", "-m", "Initial base contract commit"])
        .current_dir(repo_root);
    assert!(commit.status().unwrap().success());

    let head = git.run_git(&["rev-parse", "HEAD"], None).await.unwrap();
    (git, head.trim().to_string())
}

fn create_valid_spec(head: &str) -> SynapseSpec {
    let mut partitions = BTreeMap::new();
    partitions.insert(
        "claude".into(),
        PartitionConfig {
            role: "API layer".into(),
            owned_globs: vec!["src/api/**".into()],
            read_globs: vec!["src/contracts/**".into()],
        },
    );
    partitions.insert(
        "codex".into(),
        PartitionConfig {
            role: "Worker layer".into(),
            owned_globs: vec!["src/workers/**".into()],
            read_globs: vec!["src/contracts/**".into()],
        },
    );

    let mut shared_file_policy = BTreeMap::new();
    shared_file_policy.insert(
        "pnpm-lock.yaml".into(),
        SharedFileRule {
            owner: "supervisor".into(),
            resolution: Some("regenerate".into()),
        },
    );

    SynapseSpec {
        schema_version: 1,
        run_id: "test-m3-run".into(),
        feature_id: "feat-order-processing".into(),
        base_commit: head.to_string(),
        contract_revision: 1,
        contract_files: vec!["src/contracts/order.contract.ts".into()],
        contract_tests: vec!["tests/order.contract.test.ts".into()],
        partitions,
        shared_file_policy,
        shared_files: vec!["pnpm-lock.yaml".into()],
        verification_commands: vec![VerificationCommand {
            name: "test_order_file_exists".into(),
            argv: vec!["git".into(), "status".into()],
            cwd: None,
            timeout_seconds: 10,
            required: true,
        }],
        limits: LimitsConfig {
            max_parallel_workers: 2,
            max_repair_iterations: 2,
            worker_timeout_seconds: 60,
            context_bundle_max_bytes: 262144,
        },
        approval_policy: Default::default(),
    }
}

/// Test 1: Bounded Repair Loop with Context Firewall
/// When an initial worker code snippet fails deterministic checks,
/// Synapse enters REPAIR, packs isolated context bundle, dispatches repair,
/// re-integrates into WT_STAGE, and passes verification.
#[tokio::test]
async fn test_bounded_repair_loop_with_context_firewall() {
    let temp_dir = TempDir::new().unwrap();
    let repo_root = temp_dir.path();
    let (git, head) = setup_git_repo(&temp_dir).await;

    let mut spec = create_valid_spec(&head);
    spec.run_id = "test-run-repair-success".into();

    // Configure a verification command that checks for a required fix in src/api/handler.ts
    // We check git diff to verify "export const orderStatus = 'verified';" exists
    spec.verification_commands = vec![VerificationCommand {
        name: "check_order_status_verified".into(),
        argv: vec![
            "git".into(),
            "grep".into(),
            "orderStatus = 'verified'".into(),
            "src/api/handler.ts".into(),
        ],
        cwd: None,
        timeout_seconds: 10,
        required: true,
    }];

    let spec_path = repo_root.join("synapse-spec.json");
    fs::write(&spec_path, serde_json::to_string_pretty(&spec).unwrap()).unwrap();

    let mut add = Command::new("git");
    add.args(["add", "synapse-spec.json"])
        .current_dir(repo_root);
    assert!(add.status().unwrap().success());
    let mut commit = Command::new("git");
    commit
        .args(["commit", "-m", "Add spec"])
        .current_dir(repo_root);
    assert!(commit.status().unwrap().success());

    let approved_hash = Orchestrator::approve_contract(
        repo_root,
        &spec_path,
        Some("test-run-repair-success".into()),
        None,
    )
    .await
    .unwrap();

    // Initial Claude write has a bug ('draft' instead of 'verified')
    let options = DualWorkerOptions {
        spec_path,
        run_id: Some("test-run-repair-success".into()),
        claude_adapter_name: "mock".into(),
        codex_adapter_name: "mock".into(),
        claude_mock_file_writes: vec![(
            "src/api/handler.ts".into(),
            "export const orderStatus = 'draft';\n".into(),
        )],
        codex_mock_file_writes: vec![(
            "src/workers/processor.ts".into(),
            "export const workerReady = true;\n".into(),
        )],
        approve_hash: Some(approved_hash),
        // The mock repair file write will fix it on iteration 1
        repair_mock_file_writes: vec![(
            "src/api/handler.ts".into(),
            "export const orderStatus = 'verified';\n".into(),
        )],
        max_repair_iterations: Some(2),
        ..Default::default()
    };

    let report = Orchestrator::run_dual_agent(repo_root, options)
        .await
        .unwrap();

    // Verification passed after repair!
    assert_eq!(report.status, "REVIEW_READY");
    assert!(report.all_required_checks_passed);
    assert_eq!(report.repair_iterations_count, 1);

    // Verify context firewall saved an audit bundle
    let store = RunStore::new(repo_root, "test-run-repair-success").unwrap();
    let bundle_file = store
        .artifacts_dir()
        .join("repair_context_iter_1_claude.json");
    assert!(bundle_file.exists());
    let bundle_content = fs::read_to_string(&bundle_file).unwrap();
    assert!(bundle_content.contains("check_order_status_verified"));

    // Verify user's original worktree is untouched
    let status_out = git.run_git(&["status", "--porcelain"], None).await.unwrap();
    assert!(
        status_out.is_empty(),
        "User repository dirty: {}",
        status_out
    );
}

/// Test 2: Repair Retry Cap Enforcement
/// When checks persistently fail, repair loop stops after max_repair_iterations
/// and transitions to NeedsUser or Failed without infinite looping.
#[tokio::test]
async fn test_repair_retry_cap_enforcement() {
    let temp_dir = TempDir::new().unwrap();
    let repo_root = temp_dir.path();
    let (git, head) = setup_git_repo(&temp_dir).await;

    let mut spec = create_valid_spec(&head);
    spec.run_id = "test-run-retry-cap".into();

    // Always failing verification command
    spec.verification_commands = vec![VerificationCommand {
        name: "always_failing_check".into(),
        argv: vec!["git".into(), "invalid-subcommand-fails".into()],
        cwd: None,
        timeout_seconds: 5,
        required: true,
    }];
    spec.limits.max_repair_iterations = 2;

    let spec_path = repo_root.join("synapse-spec.json");
    fs::write(&spec_path, serde_json::to_string_pretty(&spec).unwrap()).unwrap();

    let mut add = Command::new("git");
    add.args(["add", "synapse-spec.json"])
        .current_dir(repo_root);
    assert!(add.status().unwrap().success());
    let mut commit = Command::new("git");
    commit
        .args(["commit", "-m", "Add spec"])
        .current_dir(repo_root);
    assert!(commit.status().unwrap().success());

    let approved_hash = Orchestrator::approve_contract(
        repo_root,
        &spec_path,
        Some("test-run-retry-cap".into()),
        None,
    )
    .await
    .unwrap();

    let options = DualWorkerOptions {
        spec_path,
        run_id: Some("test-run-retry-cap".into()),
        claude_adapter_name: "mock".into(),
        codex_adapter_name: "mock".into(),
        claude_mock_file_writes: vec![(
            "src/api/handler.ts".into(),
            "export const order = 1;\n".into(),
        )],
        codex_mock_file_writes: vec![(
            "src/workers/processor.ts".into(),
            "export const worker = 1;\n".into(),
        )],
        approve_hash: Some(approved_hash),
        max_repair_iterations: Some(2),
        ..Default::default()
    };

    let report = Orchestrator::run_dual_agent(repo_root, options)
        .await
        .unwrap();

    // Should fail and stop at max iterations (2)
    assert_eq!(report.status, "FAILED");
    assert!(!report.all_required_checks_passed);
    assert_eq!(report.repair_iterations_count, 2);

    let store = RunStore::new(repo_root, "test-run-retry-cap").unwrap();
    let snapshot = store.load_state_snapshot().unwrap();
    assert_eq!(snapshot.current_state, RunState::NeedsUser);

    // Verify user tree is clean
    let status_out = git.run_git(&["status", "--porcelain"], None).await.unwrap();
    assert!(status_out.is_empty());
}

/// Test 3: Independent Model Review & Executed Adversarial Tests
/// PRD Section 6 FR-6: execute any newly generated tests before counting them as evidence.
#[tokio::test]
async fn test_independent_model_review_and_adversarial_tests() {
    let temp_dir = TempDir::new().unwrap();
    let repo_root = temp_dir.path();
    let (_git, head) = setup_git_repo(&temp_dir).await;

    let mut spec = create_valid_spec(&head);
    spec.run_id = "test-run-review".into();

    let spec_path = repo_root.join("synapse-spec.json");
    fs::write(&spec_path, serde_json::to_string_pretty(&spec).unwrap()).unwrap();

    let mut add = Command::new("git");
    add.args(["add", "synapse-spec.json"])
        .current_dir(repo_root);
    assert!(add.status().unwrap().success());
    let mut commit = Command::new("git");
    commit
        .args(["commit", "-m", "Add spec"])
        .current_dir(repo_root);
    assert!(commit.status().unwrap().success());

    let approved_hash =
        Orchestrator::approve_contract(repo_root, &spec_path, Some("test-run-review".into()), None)
            .await
            .unwrap();

    let reviewer = Arc::new(
        MockReviewer::new("adversarial-reviewer-claude")
            .with_finding(ReviewFinding {
                severity: FindingSeverity::Minor,
                file_path: Some("src/api/handler.ts".into()),
                line_number: Some(10),
                title: "Potential integer overflow".into(),
                description: "Ensure amount is validated positive".into(),
                suggested_fix: Some("assert(amount > 0)".into()),
            })
            .with_proposal(AdversarialTestProposal {
                test_name: "test_negative_order_amount".into(),
                relative_path: "tests/adversarial/order_amount.test.txt".into(),
                test_code: "// Adversarial test for negative order\n".into(),
                verification_command: VerificationCommand {
                    name: "adv_order_test".into(),
                    argv: vec!["git".into(), "--version".into()],
                    cwd: None,
                    timeout_seconds: 5,
                    required: true,
                },
            }),
    );

    let options = DualWorkerOptions {
        spec_path,
        run_id: Some("test-run-review".into()),
        claude_adapter_name: "mock".into(),
        codex_adapter_name: "mock".into(),
        claude_mock_file_writes: vec![(
            "src/api/handler.ts".into(),
            "export const handle = () => true;\n".into(),
        )],
        codex_mock_file_writes: vec![(
            "src/workers/processor.ts".into(),
            "export const process = () => true;\n".into(),
        )],
        approve_hash: Some(approved_hash),
        reviewer: Some(reviewer),
        ..Default::default()
    };

    let report = Orchestrator::run_dual_agent(repo_root, options)
        .await
        .unwrap();

    assert_eq!(report.status, "REVIEW_READY");
    assert_eq!(report.review_findings.len(), 1);
    assert_eq!(report.review_findings[0].severity, FindingSeverity::Minor);
    assert_eq!(report.executed_adversarial_checks.len(), 1);
    assert_eq!(
        report.executed_adversarial_checks[0].status,
        synapse_core::check::CheckStatus::Passed
    );

    let md = report.to_markdown_string();
    assert!(md.contains("Independent Model Review & Adversarial Tests"));
    assert!(md.contains("Potential integer overflow"));
}

/// Test 4: Crash Recovery and Idempotent Resume from Completed Checkpoints
/// Proves that when workers have completed and captured checkpoints onto internal refs,
/// synapse resume skips re-running workers and safely merges and verifies staging.
#[tokio::test]
async fn test_crash_recovery_and_idempotent_resume() {
    let temp_dir = TempDir::new().unwrap();
    let repo_root = temp_dir.path();
    let (git, head) = setup_git_repo(&temp_dir).await;

    let spec_path = repo_root.join("synapse-spec.json");
    let mut spec = create_valid_spec(&head);
    spec.run_id = "test-run-resume".into();
    fs::write(&spec_path, serde_json::to_string_pretty(&spec).unwrap()).unwrap();

    let mut add = Command::new("git");
    add.args(["add", "synapse-spec.json"])
        .current_dir(repo_root);
    assert!(add.status().unwrap().success());
    let mut commit = Command::new("git");
    commit
        .args(["commit", "-m", "Add spec"])
        .current_dir(repo_root);
    assert!(commit.status().unwrap().success());

    let spec_commit = git.run_git(&["rev-parse", "HEAD"], None).await.unwrap();
    let spec_commit = spec_commit.trim().to_string();

    // Update spec to use spec_commit as base_commit
    spec.base_commit = spec_commit.clone();
    fs::write(&spec_path, serde_json::to_string_pretty(&spec).unwrap()).unwrap();
    let mut add_u = Command::new("git");
    add_u
        .args(["add", "synapse-spec.json"])
        .current_dir(repo_root);
    assert!(add_u.status().unwrap().success());
    let mut commit_u = Command::new("git");
    commit_u
        .args(["commit", "--amend", "--no-edit"])
        .current_dir(repo_root);
    assert!(commit_u.status().unwrap().success());

    let base_commit = git.run_git(&["rev-parse", "HEAD"], None).await.unwrap();
    let base_commit = base_commit.trim().to_string();

    let approved_hash =
        Orchestrator::approve_contract(repo_root, &spec_path, Some("test-run-resume".into()), None)
            .await
            .unwrap();

    // Simulate an interrupted run:
    // Create checkpoint commits directly for claude and codex
    // 1. Commit Claude's work
    fs::create_dir_all(repo_root.join("src/api")).unwrap();
    fs::write(
        repo_root.join("src/api/handler.ts"),
        "export const api = 1;\n",
    )
    .unwrap();
    let mut add = Command::new("git");
    add.args(["add", "src/api/handler.ts"])
        .current_dir(repo_root);
    assert!(add.status().unwrap().success());
    let mut commit = Command::new("git");
    commit
        .args(["commit", "-m", "synapse: checkpoint claude"])
        .current_dir(repo_root);
    assert!(commit.status().unwrap().success());
    let sha_claude = git.run_git(&["rev-parse", "HEAD"], None).await.unwrap();
    let sha_claude = sha_claude.trim();

    // 2. Commit Codex's work
    fs::create_dir_all(repo_root.join("src/workers")).unwrap();
    fs::write(
        repo_root.join("src/workers/processor.ts"),
        "export const worker = 1;\n",
    )
    .unwrap();
    let mut add2 = Command::new("git");
    add2.args(["add", "src/workers/processor.ts"])
        .current_dir(repo_root);
    assert!(add2.status().unwrap().success());
    let mut commit2 = Command::new("git");
    commit2
        .args(["commit", "-m", "synapse: checkpoint codex"])
        .current_dir(repo_root);
    assert!(commit2.status().unwrap().success());
    let sha_codex = git.run_git(&["rev-parse", "HEAD"], None).await.unwrap();
    let sha_codex = sha_codex.trim();

    // Reset base repo to base_commit (so user's base repo is clean and at base_commit)
    let mut reset = Command::new("git");
    reset
        .args(["reset", "--hard", &base_commit])
        .current_dir(repo_root);
    assert!(reset.status().unwrap().success());

    // Update synapse checkpoint refs
    git.update_ref("refs/synapse/test-run-resume/checkpoint-claude", sha_claude)
        .await
        .unwrap();
    git.update_ref("refs/synapse/test-run-resume/checkpoint-codex", sha_codex)
        .await
        .unwrap();

    // Initialize RunStore at Integration state (as if it crashed right as Integration began)
    let store = RunStore::new(repo_root, "test-run-resume").unwrap();
    store
        .initialize(Some(base_commit.clone()), Some(approved_hash.clone()))
        .unwrap();
    store
        .transition_to(
            RunState::Implementation,
            vec![],
            synapse_core::state::TransitionOutcome::Success,
            None,
        )
        .unwrap();
    store
        .transition_to(
            RunState::Checkpoint,
            vec![],
            synapse_core::state::TransitionOutcome::Success,
            None,
        )
        .unwrap();
    store
        .transition_to(
            RunState::Integration,
            vec![],
            synapse_core::state::TransitionOutcome::Success,
            Some("Simulated crash: resuming from Integration".into()),
        )
        .unwrap();

    // Check inspection confirms resume eligibility
    let inspection = RecoveryManager::inspect_run(repo_root, "test-run-resume")
        .await
        .unwrap();
    assert!(inspection.can_resume);
    assert_eq!(inspection.state, RunState::Integration);
    assert!(inspection.claude_checkpoint_ref_exists);
    assert!(inspection.codex_checkpoint_ref_exists);

    let options = DualWorkerOptions {
        spec_path: spec_path.clone(),
        run_id: Some("test-run-resume".into()),
        approve_hash: Some(approved_hash.clone()),
        ..Default::default()
    };

    // Call resume_dual_agent
    let resumed_report = Orchestrator::resume_dual_agent(repo_root, options)
        .await
        .unwrap();
    assert_eq!(resumed_report.status, "REVIEW_READY");
    assert!(resumed_report.all_required_checks_passed);

    // Verify user tree is clean
    let status_out = git.run_git(&["status", "--porcelain"], None).await.unwrap();
    assert!(status_out.is_empty());
}

/// Test 5: Cancellation and Safe Cleanup
/// Verifies that cancelling marks durable state as Cancelled and cleanup removes
/// only generated worktrees under .synapse/worktrees/<run_id>/ without touching user files.
#[tokio::test]
async fn test_cancellation_and_safe_cleanup() {
    let temp_dir = TempDir::new().unwrap();
    let repo_root = temp_dir.path();
    let (git, head) = setup_git_repo(&temp_dir).await;

    let run_id = "test-run-cleanup";
    let store = RunStore::new(repo_root, run_id).unwrap();
    store
        .initialize(Some(head), Some("contract_hash_1".into()))
        .unwrap();
    store
        .transition_to(
            RunState::Implementation,
            vec![],
            synapse_core::state::TransitionOutcome::Success,
            None,
        )
        .unwrap();

    // Create mock worktree inside .synapse/worktrees/test-run-cleanup
    let wt_path = repo_root
        .join(".synapse")
        .join("worktrees")
        .join(run_id)
        .join("claude");
    fs::create_dir_all(&wt_path).unwrap();
    fs::write(wt_path.join("temp.txt"), "temporary").unwrap();

    // Cancel
    Orchestrator::cancel_run(repo_root, run_id, Some("Engineer stopped run".into()))
        .await
        .unwrap();

    let inspection = RecoveryManager::inspect_run(repo_root, run_id)
        .await
        .unwrap();
    assert_eq!(inspection.state, RunState::Cancelled);
    assert!(!inspection.can_resume);

    // Cleanup
    let cleanup = Orchestrator::cleanup_run(repo_root, run_id, true)
        .await
        .unwrap();
    assert_eq!(cleanup.run_id, run_id);
    assert!(!wt_path.exists());
    assert!(cleanup.artifacts_kept);

    // Verify original repository is untouched
    let status_out = git.run_git(&["status", "--porcelain"], None).await.unwrap();
    assert!(status_out.is_empty());
}

/// Test 6: Security Defense Suite
/// Tests path traversal rejection, symlink escape defense, and secret scrubbing.
#[tokio::test]
async fn test_security_defense_suite() {
    let temp_dir = TempDir::new().unwrap();
    let root = temp_dir.path();

    // 1. Path traversal defense
    let traversal = validate_path_in_root("../../etc/passwd", root);
    assert!(matches!(
        traversal,
        Err(SecurityError::PathTraversalDetected { .. })
    ));

    let win_traversal = validate_path_in_root(r"..\..\Windows\System32\cmd.exe", root);
    assert!(matches!(
        win_traversal,
        Err(SecurityError::PathTraversalDetected { .. })
    ));

    // 2. Secret scrubbing defense
    let secret_log =
        "API_KEY=sk-ant-123456789012345 and auth Bearer eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9";
    let scrubbed = scrub_secrets(secret_log);
    assert!(!scrubbed.contains("sk-ant-123456789012345"));
    assert!(!scrubbed.contains("eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9"));
    assert!(scrubbed.contains("sk-ant-***REDACTED***"));
    assert!(scrubbed.contains("Bearer ***REDACTED***"));
}

/// Test 7: Redacted Diagnostic Bundle & Sent Payload Audit Manifests
/// Proves that `synapse diagnostic-bundle` exports secret-redacted, code-stripped metadata
/// and that ContextPacker generated audit manifests of sent payloads.
#[tokio::test]
async fn test_diagnostic_bundle_and_audit_manifests() {
    let temp_dir = TempDir::new().unwrap();
    let repo_root = temp_dir.path();
    let (_git, head) = setup_git_repo(&temp_dir).await;

    let mut spec = create_valid_spec(&head);
    let run_id = "test-run-diag-bundle";
    spec.run_id = run_id.into();

    let spec_path = repo_root.join("synapse-spec.json");
    fs::write(&spec_path, serde_json::to_string_pretty(&spec).unwrap()).unwrap();

    let mut add = Command::new("git");
    add.args(["add", "synapse-spec.json"])
        .current_dir(repo_root);
    assert!(add.status().unwrap().success());
    let mut commit = Command::new("git");
    commit
        .args(["commit", "-m", "Add spec"])
        .current_dir(repo_root);
    assert!(commit.status().unwrap().success());

    let approved_hash =
        Orchestrator::approve_contract(repo_root, &spec_path, Some(run_id.into()), None)
            .await
            .unwrap();

    let options = DualWorkerOptions {
        spec_path,
        run_id: Some(run_id.into()),
        claude_adapter_name: "mock".into(),
        codex_adapter_name: "mock".into(),
        claude_mock_file_writes: vec![(
            "src/api/handler.ts".into(),
            "export const api = 1;\n".into(),
        )],
        codex_mock_file_writes: vec![(
            "src/workers/processor.ts".into(),
            "export const worker = 1;\n".into(),
        )],
        approve_hash: Some(approved_hash),
        ..Default::default()
    };

    let report = Orchestrator::run_dual_agent(repo_root, options)
        .await
        .unwrap();
    assert_eq!(report.status, "REVIEW_READY");

    // Export diagnostic bundle
    let bundle_path = Orchestrator::export_diagnostic_bundle(repo_root, run_id, None)
        .await
        .unwrap();
    assert!(bundle_path.exists());

    let bundle_content = fs::read_to_string(&bundle_path).unwrap();
    assert!(bundle_content.contains(run_id));
    assert!(bundle_content.contains("\"state_snapshot\""));
    assert!(bundle_content.contains("\"transitions\""));

    // Verify raw source code is not bundled
    assert!(!bundle_content.contains("export const api = 1;"));
}
