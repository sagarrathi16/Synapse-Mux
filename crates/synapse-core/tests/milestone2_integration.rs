use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::process::Command;

use synapse_core::git::GitManager;
use synapse_core::manifest::{
    LimitsConfig, PartitionConfig, SharedFileRule, SynapseSpec, VerificationCommand,
};
use synapse_core::runner::{DualWorkerOptions, Orchestrator};
use synapse_core::state::{RunState, RunStore};
use tempfile::TempDir;

fn init_git_repo(path: &Path) {
    let run = |args: &[&str]| {
        let mut cmd = Command::new("git");
        cmd.args(args).current_dir(path);
        let status = cmd.status().expect("git command failed");
        assert!(status.success(), "Git command failed: {:?}", args);
    };

    run(&["init"]);
    run(&["config", "user.name", "Synapse M2 CI"]);
    run(&["config", "user.email", "ci@synapse.local"]);

    fs::write(path.join(".gitignore"), b".synapse/\n").unwrap();
    run(&["add", ".gitignore"]);

    fs::create_dir_all(path.join("src").join("contracts")).unwrap();
    fs::write(
        path.join("src").join("contracts").join("webhook-events.ts"),
        b"export interface WebhookEvent { id: string; timestamp: number; }\n",
    )
    .unwrap();
    run(&["add", "src/contracts/webhook-events.ts"]);

    fs::create_dir_all(path.join("tests")).unwrap();
    fs::write(
        path.join("tests").join("webhook.contract.test.ts"),
        b"// contract tests for webhook boundaries\n",
    )
    .unwrap();
    run(&["add", "tests/webhook.contract.test.ts"]);

    fs::write(path.join("pnpm-lock.yaml"), b"# lockfile v1\n").unwrap();
    run(&["add", "pnpm-lock.yaml"]);

    run(&["commit", "-m", "Initial base contract commit"]);
}

#[tokio::test]
async fn test_spec_validation_rejects_overlapping_globs_and_path_traversal() {
    let temp = TempDir::new().unwrap();
    let repo_root = temp.path();
    init_git_repo(repo_root);

    let git_mgr = GitManager::new(repo_root).unwrap();
    let head = git_mgr.get_head_commit().await.unwrap();

    // 1. Overlapping globs rejection
    let mut partitions = BTreeMap::new();
    partitions.insert(
        "claude".into(),
        PartitionConfig {
            role: "API".into(),
            owned_globs: vec!["src/api/**".into()],
            read_globs: vec![],
        },
    );
    partitions.insert(
        "codex".into(),
        PartitionConfig {
            role: "Worker".into(),
            owned_globs: vec!["src/api/webhooks/**".into()], // Overlaps with src/api/**
            read_globs: vec![],
        },
    );

    let spec_overlap = SynapseSpec {
        schema_version: 1,
        run_id: "test-overlap".into(),
        feature_id: "feat-test".into(),
        base_commit: head.clone(),
        contract_revision: 1,
        contract_files: vec!["src/contracts/webhook-events.ts".into()],
        contract_tests: vec!["tests/webhook.contract.test.ts".into()],
        partitions,
        shared_file_policy: BTreeMap::new(),
        shared_files: vec![],
        verification_commands: vec![],
        limits: LimitsConfig::default(),
        approval_policy: Default::default(),
    };

    let spec_overlap_path = repo_root.join("spec_overlap.json");
    fs::write(
        &spec_overlap_path,
        serde_json::to_string_pretty(&spec_overlap).unwrap(),
    )
    .unwrap();

    let options = DualWorkerOptions {
        spec_path: spec_overlap_path,
        claude_adapter_name: "mock".into(),
        codex_adapter_name: "mock".into(),
        ..Default::default()
    };

    let res = Orchestrator::run_dual_agent(repo_root, options).await;
    assert!(res.is_err(), "Should reject overlapping globs");
    let err_str = res.unwrap_err().to_string();
    assert!(err_str.contains("Overlapping partition write ownership"));

    // 2. Path traversal rejection
    let spec_traversal = SynapseSpec {
        schema_version: 1,
        run_id: "test-traversal".into(),
        feature_id: "feat-test".into(),
        base_commit: head,
        contract_revision: 1,
        contract_files: vec!["../../etc/passwd".into()],
        contract_tests: vec![],
        partitions: BTreeMap::new(),
        shared_file_policy: BTreeMap::new(),
        shared_files: vec![],
        verification_commands: vec![],
        limits: LimitsConfig::default(),
        approval_policy: Default::default(),
    };

    let spec_traversal_path = repo_root.join("spec_traversal.json");
    fs::write(
        &spec_traversal_path,
        serde_json::to_string_pretty(&spec_traversal).unwrap(),
    )
    .unwrap();

    let options_traversal = DualWorkerOptions {
        spec_path: spec_traversal_path,
        claude_adapter_name: "mock".into(),
        codex_adapter_name: "mock".into(),
        ..Default::default()
    };

    let res2 = Orchestrator::run_dual_agent(repo_root, options_traversal).await;
    assert!(res2.is_err(), "Should reject path traversal");
    let err_str2 = res2.unwrap_err().to_string();
    assert!(err_str2.contains("Path traversal detected"));
}

#[tokio::test]
async fn test_out_of_partition_and_contract_mutation_caught_at_checkpoint() {
    let temp = TempDir::new().unwrap();
    let repo_root = temp.path();
    init_git_repo(repo_root);

    let git_mgr = GitManager::new(repo_root).unwrap();
    let head = git_mgr.get_head_commit().await.unwrap();

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

    let spec = SynapseSpec {
        schema_version: 1,
        run_id: "test-run-violation".into(),
        feature_id: "feat-boundary-test".into(),
        base_commit: head,
        contract_revision: 1,
        contract_files: vec!["src/contracts/webhook-events.ts".into()],
        contract_tests: vec!["tests/webhook.contract.test.ts".into()],
        partitions,
        shared_file_policy,
        shared_files: vec!["pnpm-lock.yaml".into()],
        verification_commands: vec![],
        limits: LimitsConfig::default(),
        approval_policy: Default::default(),
    };

    let spec_path = repo_root.join("synapse-spec.json");
    fs::write(&spec_path, serde_json::to_string_pretty(&spec).unwrap()).unwrap();

    // Commit spec so working tree is clean
    let mut cmd = Command::new("git");
    cmd.args(["add", "synapse-spec.json"])
        .current_dir(repo_root);
    assert!(cmd.status().unwrap().success());
    let mut cmd2 = Command::new("git");
    cmd2.args(["commit", "-m", "Add spec"])
        .current_dir(repo_root);
    assert!(cmd2.status().unwrap().success());

    // Worker A (Claude) writes to src/workers/trespass.ts (owned by Codex!)
    let options = DualWorkerOptions {
        spec_path: spec_path.clone(),
        run_id: Some("test-run-violation".into()),
        claude_adapter_name: "mock".into(),
        codex_adapter_name: "mock".into(),
        claude_mock_file_writes: vec![(
            "src/workers/trespass.ts".into(),
            "export const hacker = true;\n".into(),
        )],
        codex_mock_file_writes: vec![(
            "src/workers/clean.ts".into(),
            "export const clean = true;\n".into(),
        )],
        ..Default::default()
    };

    let res = Orchestrator::run_dual_agent(repo_root, options).await;
    assert!(res.is_err(), "Must halt on partition violation!");
    let err_msg = res.unwrap_err().to_string();
    assert!(err_msg.contains("Partition ownership violation"));
    assert!(err_msg.contains("Worker 'claude' out_of_partition in 'src/workers/trespass.ts'"));

    // Check that violation diff was archived
    let store = RunStore::new(repo_root, "test-run-violation").unwrap();
    let snapshot = store.load_state_snapshot().unwrap();
    assert_eq!(snapshot.current_state, RunState::Failed);

    let violation_diff_path = store
        .run_dir()
        .join("violations")
        .join("claude_src_workers_trespass.ts.diff");
    assert!(
        violation_diff_path.exists(),
        "Offending diff must be archived in violations/"
    );

    // Verify user's original repo remains 100% clean and untouched
    let (is_clean, dirty_status) = git_mgr.check_clean_working_tree().await.unwrap();
    assert!(is_clean, "Original repo was dirtied: {}", dirty_status);
}

#[tokio::test]
async fn test_clean_dual_worker_partitions_merge_into_stage_and_pass_verification() {
    let temp = TempDir::new().unwrap();
    let repo_root = temp.path();
    init_git_repo(repo_root);

    let git_mgr = GitManager::new(repo_root).unwrap();
    let head = git_mgr.get_head_commit().await.unwrap();

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

    let spec = SynapseSpec {
        schema_version: 1,
        run_id: "test-run-clean-dual".into(),
        feature_id: "feat-webhook-system".into(),
        base_commit: head,
        contract_revision: 1,
        contract_files: vec!["src/contracts/webhook-events.ts".into()],
        contract_tests: vec!["tests/webhook.contract.test.ts".into()],
        partitions,
        shared_file_policy,
        shared_files: vec!["pnpm-lock.yaml".into()],
        verification_commands: vec![VerificationCommand {
            name: "verify_repo_state".into(),
            argv: vec!["git".into(), "status".into()],
            cwd: None,
            timeout_seconds: 10,
            required: true,
        }],
        limits: LimitsConfig::default(),
        approval_policy: Default::default(),
    };

    let spec_path = repo_root.join("synapse-spec.json");
    fs::write(&spec_path, serde_json::to_string_pretty(&spec).unwrap()).unwrap();

    // Commit spec
    let mut cmd = Command::new("git");
    cmd.args(["add", "synapse-spec.json"])
        .current_dir(repo_root);
    assert!(cmd.status().unwrap().success());
    let mut cmd2 = Command::new("git");
    cmd2.args(["commit", "-m", "Add spec"])
        .current_dir(repo_root);
    assert!(cmd2.status().unwrap().success());

    // Step 1: Approve Contract Gate
    let approved_hash = Orchestrator::approve_contract(
        repo_root,
        &spec_path,
        Some("test-run-clean-dual".into()),
        None,
    )
    .await
    .unwrap();
    assert!(!approved_hash.is_empty());

    // Step 2: Run Dual Agent Execution
    let options = DualWorkerOptions {
        spec_path,
        run_id: Some("test-run-clean-dual".into()),
        claude_adapter_name: "mock".into(),
        codex_adapter_name: "mock".into(),
        claude_mock_file_writes: vec![(
            "src/api/handler.ts".into(),
            "export const handleWebhook = () => { return 'handled'; };\n".into(),
        )],
        codex_mock_file_writes: vec![(
            "src/workers/processor.ts".into(),
            "export const processWebhook = () => { return 'processed'; };\n".into(),
        )],
        approve_hash: Some(approved_hash),
        ..Default::default()
    };

    let report = Orchestrator::run_dual_agent(repo_root, options)
        .await
        .unwrap();

    // Verify report
    assert_eq!(report.status, "REVIEW_READY");
    assert!(report.all_required_checks_passed);
    assert!(
        report
            .changed_files
            .contains(&"src/api/handler.ts".to_string())
    );
    assert!(
        report
            .changed_files
            .contains(&"src/workers/processor.ts".to_string())
    );
    assert!(report.unified_diff.contains("+export const handleWebhook"));
    assert!(report.unified_diff.contains("+export const processWebhook"));

    // Verify state machine reached REVIEW_READY
    let store = RunStore::new(repo_root, "test-run-clean-dual").unwrap();
    let snapshot = store.load_state_snapshot().unwrap();
    assert_eq!(snapshot.current_state, RunState::ReviewReady);

    let transitions = store.list_transitions().unwrap();
    assert!(
        transitions
            .iter()
            .any(|t| t.to_state == RunState::Integration)
    );
    assert!(
        transitions
            .iter()
            .any(|t| t.to_state == RunState::Verification)
    );
    assert_eq!(transitions.last().unwrap().to_state, RunState::ReviewReady);

    // Verify checkpoint refs were updated
    let claude_ref_out = git_mgr
        .run_git(
            &[
                "rev-parse",
                "refs/synapse/test-run-clean-dual/checkpoint-claude",
            ],
            None,
        )
        .await;
    assert!(claude_ref_out.is_ok());

    let codex_ref_out = git_mgr
        .run_git(
            &[
                "rev-parse",
                "refs/synapse/test-run-clean-dual/checkpoint-codex",
            ],
            None,
        )
        .await;
    assert!(codex_ref_out.is_ok());

    // CRITICAL INVARIANT: Original repository remains 100% clean and untouched
    let (is_clean, dirty_status) = git_mgr.check_clean_working_tree().await.unwrap();
    assert!(
        is_clean,
        "Original repo was dirtied! Status:\n{}",
        dirty_status
    );
    assert!(
        !repo_root
            .join("src")
            .join("api")
            .join("handler.ts")
            .exists()
    );
    assert!(
        !repo_root
            .join("src")
            .join("workers")
            .join("processor.ts")
            .exists()
    );
}
