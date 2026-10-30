use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use synapse_core::git::GitManager;
use synapse_core::runner::{Orchestrator, SingleWorkerOptions};
use synapse_core::state::{RunState, RunStore};
use tempfile::TempDir;

fn init_test_git_repo(path: &Path) {
    let run = |args: &[&str]| {
        let mut cmd = Command::new("git");
        cmd.args(args).current_dir(path);
        let status = cmd.status().expect("git command failed");
        assert!(status.success(), "Git command failed: {:?}", args);
    };

    run(&["init"]);
    run(&["config", "user.name", "Synapse CI"]);
    run(&["config", "user.email", "ci@synapse.local"]);

    // Create .gitignore
    fs::write(path.join(".gitignore"), b".synapse/\n").unwrap();
    run(&["add", ".gitignore"]);

    // Create base code file
    fs::create_dir_all(path.join("src")).unwrap();
    fs::write(
        path.join("src").join("index.ts"),
        b"export const greeting = 'hello';\n",
    )
    .unwrap();
    run(&["add", "src/index.ts"]);
    run(&["commit", "-m", "Initial commit"]);
}

#[tokio::test]
async fn test_e2e_single_agent_success_leaves_original_tree_untouched() {
    let temp = TempDir::new().unwrap();
    let repo_root = temp.path();
    init_test_git_repo(repo_root);

    let git_mgr = GitManager::new(repo_root).unwrap();
    let head_commit = git_mgr.get_head_commit().await.unwrap();

    // Create a synapse-spec.json
    let spec_json = serde_json::json!({
        "schema_version": 1,
        "run_id": "test-run-e2e-success",
        "feature_id": "feat-greeting-update",
        "base_commit": head_commit,
        "contract_files": [],
        "contract_tests": [],
        "partitions": {},
        "shared_file_policy": {},
        "shared_files": [],
        "verification_commands": [
            {
                "name": "verify_feature_file",
                "argv": ["git", "status"],
                "timeout_seconds": 10,
                "required": true
            }
        ],
        "limits": {
            "max_parallel_workers": 1,
            "max_repair_iterations": 1,
            "worker_timeout_seconds": 60,
            "context_bundle_max_bytes": 100000
        },
        "approval_policy": {}
    });

    let spec_path = repo_root.join("synapse-spec.json");
    fs::write(
        &spec_path,
        serde_json::to_string_pretty(&spec_json).unwrap(),
    )
    .unwrap();

    // Commit spec so working tree is clean for preflight
    let run = |args: &[&str]| {
        let mut cmd = Command::new("git");
        cmd.args(args).current_dir(repo_root);
        let status = cmd.status().expect("git failed");
        assert!(status.success());
    };
    run(&["add", "synapse-spec.json"]);
    run(&["commit", "-m", "Add synapse-spec.json"]);

    let head_commit_with_spec = git_mgr.get_head_commit().await.unwrap();

    let options = SingleWorkerOptions {
        spec_path: Some(spec_path),
        run_id: Some("test-run-e2e-success".into()),
        adapter_name: "mock".into(),
        mock_file_writes: vec![(
            "src/feature.ts".into(),
            "export const feature = 'active';\n".into(),
        )],
        ..Default::default()
    };

    let report = Orchestrator::run_single_agent(repo_root, options)
        .await
        .unwrap();

    // 1. Report is REVIEW_READY and all checks passed
    assert_eq!(report.status, "REVIEW_READY");
    assert!(report.all_required_checks_passed);
    assert_eq!(report.checks.len(), 1);
    assert_eq!(report.changed_files, vec!["src/feature.ts"]);
    assert!(
        report
            .unified_diff
            .contains("+export const feature = 'active';")
    );

    // 2. CRITICAL GATE: Original working tree must be 100% clean and untouched!
    let (is_clean, dirty_status) = git_mgr.check_clean_working_tree().await.unwrap();
    assert!(
        is_clean,
        "Original repo was dirtied by runner! Dirty status: {}",
        dirty_status
    );
    assert_eq!(
        git_mgr.get_head_commit().await.unwrap(),
        head_commit_with_spec
    );
    assert!(
        !repo_root.join("src").join("feature.ts").exists(),
        "Worker file leaked into user working tree!"
    );

    // 3. Artifact verification
    let store = RunStore::new(repo_root, "test-run-e2e-success").unwrap();
    let snapshot = store.load_state_snapshot().unwrap();
    assert_eq!(snapshot.current_state, RunState::ReviewReady);

    let transitions = store.list_transitions().unwrap();
    assert!(transitions.len() >= 5);
    assert_eq!(transitions.last().unwrap().to_state, RunState::ReviewReady);

    assert!(store.artifacts_dir().join("report.json").exists());
    assert!(store.artifacts_dir().join("report.md").exists());
    assert!(store.artifacts_dir().join("checkpoint.json").exists());
    assert!(store.artifacts_dir().join("patch.diff").exists());
    assert!(store.artifacts_dir().join("events.jsonl").exists());
}

#[tokio::test]
async fn test_e2e_worker_timeout_leaves_original_tree_untouched() {
    let temp = TempDir::new().unwrap();
    let repo_root = temp.path();
    init_test_git_repo(repo_root);

    let git_mgr = GitManager::new(repo_root).unwrap();

    let options = SingleWorkerOptions {
        run_id: Some("test-run-timeout".into()),
        adapter_name: "mock".into(),
        mock_timeout: true,
        ..Default::default()
    };

    let report = Orchestrator::run_single_agent(repo_root, options)
        .await
        .unwrap();

    // Run must transition to FAILED
    assert_eq!(report.status, "FAILED");

    // Original repo is STILL clean
    let (is_clean, dirty_status) = git_mgr.check_clean_working_tree().await.unwrap();
    assert!(
        is_clean,
        "Original repo was dirtied on timeout: {}",
        dirty_status
    );

    // State machine is FAILED
    let store = RunStore::new(repo_root, "test-run-timeout").unwrap();
    let snapshot = store.load_state_snapshot().unwrap();
    assert_eq!(snapshot.current_state, RunState::Failed);
}

#[tokio::test]
async fn test_e2e_malformed_jsonl_event_triggers_failure() {
    let temp = TempDir::new().unwrap();
    let repo_root = temp.path();
    init_test_git_repo(repo_root);

    let git_mgr = GitManager::new(repo_root).unwrap();

    let fixture_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures/claude/malformed.jsonl");

    let options = SingleWorkerOptions {
        run_id: Some("test-run-malformed".into()),
        adapter_name: "mock".into(),
        mock_fixture: Some(fixture_path),
        ..Default::default()
    };

    let report = Orchestrator::run_single_agent(repo_root, options)
        .await
        .unwrap();

    // Must fail due to malformed events
    assert_eq!(report.status, "FAILED");

    // Diagnostics retained
    let store = RunStore::new(repo_root, "test-run-malformed").unwrap();
    let snapshot = store.load_state_snapshot().unwrap();
    assert_eq!(snapshot.current_state, RunState::Failed);

    let events_content = fs::read_to_string(store.artifacts_dir().join("events.jsonl")).unwrap();
    assert!(events_content.contains("MalformedEvent"));

    // Original working tree untouched
    let (is_clean, _) = git_mgr.check_clean_working_tree().await.unwrap();
    assert!(is_clean);
}
