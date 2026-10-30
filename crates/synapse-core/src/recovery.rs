use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use crate::git::GitManager;
use crate::runner::RunnerError;
use crate::state::{RunState, RunStore, TransitionOutcome};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunInspection {
    pub run_id: String,
    pub state: RunState,
    pub base_commit: Option<String>,
    pub contract_hash: Option<String>,
    pub claude_checkpoint_ref_exists: bool,
    pub codex_checkpoint_ref_exists: bool,
    pub worktrees_on_disk: Vec<String>,
    pub can_resume: bool,
    pub reconciliation_required: bool,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CleanupSummary {
    pub run_id: String,
    pub removed_worktrees: Vec<PathBuf>,
    pub pruned_git_worktrees: bool,
    pub artifacts_kept: bool,
}

pub struct RecoveryManager;

impl RecoveryManager {
    /// Inspects the durable state of a run and determines resume eligibility
    pub async fn inspect_run(
        repo_root: impl AsRef<Path>,
        run_id: &str,
    ) -> Result<RunInspection, RunnerError> {
        let repo_root = repo_root.as_ref();
        let store = RunStore::new(repo_root, run_id)?;
        let git_mgr = GitManager::new(repo_root)?;

        let snapshot = store.load_state_snapshot()?;
        let current_state = snapshot.current_state;
        let base_commit = snapshot.base_commit.clone();
        let contract_hash = snapshot.contract_hash.clone();

        // Check git refs for completed checkpoints
        let ref_claude = format!("refs/synapse/{run_id}/checkpoint-claude");
        let ref_codex = format!("refs/synapse/{run_id}/checkpoint-codex");

        let claude_ref_exists = git_mgr
            .run_git(&["rev-parse", "--verify", &ref_claude], None)
            .await
            .is_ok();
        let codex_ref_exists = git_mgr
            .run_git(&["rev-parse", "--verify", &ref_codex], None)
            .await
            .is_ok();

        // Inspect worktrees on disk
        let worktrees_root = repo_root.join(".synapse").join("worktrees").join(run_id);
        let mut worktrees_on_disk = Vec::new();
        if worktrees_root.exists()
            && let Ok(entries) = fs::read_dir(&worktrees_root)
        {
            for entry in entries.flatten() {
                if let Ok(ft) = entry.file_type()
                    && ft.is_dir()
                {
                    worktrees_on_disk.push(entry.file_name().to_string_lossy().to_string());
                }
            }
        }

        // Check if repository HEAD matches base commit
        let mut reconciliation_required = false;
        if let Some(ref expected_base) = base_commit
            && let Ok(head_sha) = git_mgr.run_git(&["rev-parse", "HEAD"], None).await
        {
            let trimmed_head = head_sha.trim();
            if trimmed_head != expected_base {
                reconciliation_required = true;
            }
        }

        let (can_resume, reason) = match current_state {
            RunState::ReviewReady => (
                false,
                "Run has already reached ReviewReady terminal state".into(),
            ),
            RunState::Failed => (
                false,
                "Run failed and cannot be resumed without explicit repair or restart".into(),
            ),
            RunState::Cancelled => (false, "Run was cancelled by engineer".into()),
            RunState::NeedsUser => (
                true,
                "Awaiting engineer action; can resume after resolving conflicts".into(),
            ),
            RunState::Implementation => {
                if claude_ref_exists && codex_ref_exists {
                    (
                        true,
                        "Both worker checkpoints exist; can resume from Integration".into(),
                    )
                } else {
                    (true, "Can resume active implementation".into())
                }
            }
            RunState::Checkpoint
            | RunState::Integration
            | RunState::Verification
            | RunState::Repair => (true, format!("Can resume from stage {current_state}")),
            RunState::Preflight | RunState::ContractProposal | RunState::ContractApproval => (
                true,
                format!("Can resume from initial phase {current_state}"),
            ),
        };

        Ok(RunInspection {
            run_id: run_id.to_string(),
            state: current_state,
            base_commit,
            contract_hash,
            claude_checkpoint_ref_exists: claude_ref_exists,
            codex_checkpoint_ref_exists: codex_ref_exists,
            worktrees_on_disk,
            can_resume,
            reconciliation_required,
            reason,
        })
    }

    /// Cancels an in-flight run, marks durable state as Cancelled, and preserves artifacts
    pub async fn cancel_run(
        repo_root: impl AsRef<Path>,
        run_id: &str,
        reason: Option<String>,
    ) -> Result<(), RunnerError> {
        let repo_root = repo_root.as_ref();
        let store = RunStore::new(repo_root, run_id)?;
        let git_mgr = GitManager::new(repo_root)?;

        let cancel_reason = reason.unwrap_or_else(|| "Run cancelled by user request".into());

        info!("Cancelling run '{}': {}", run_id, cancel_reason);

        // Transition durable state to Cancelled
        store.transition_to(
            RunState::Cancelled,
            vec![],
            TransitionOutcome::Failure(cancel_reason.clone()),
            Some(format!("Cancelled: {cancel_reason}")),
        )?;

        // Safely prune any remaining worktrees for this run
        let worktrees_dir = repo_root.join(".synapse").join("worktrees").join(run_id);
        if worktrees_dir.exists() {
            let _ = git_mgr.run_git(&["worktree", "prune"], None).await;
        }

        Ok(())
    }

    /// Cleans up generated worktrees safely. Never deletes user's original worktree!
    pub async fn cleanup_run(
        repo_root: impl AsRef<Path>,
        run_id: &str,
        keep_artifacts: bool,
    ) -> Result<CleanupSummary, RunnerError> {
        let repo_root = repo_root.as_ref();
        let git_mgr = GitManager::new(repo_root)?;

        let run_worktrees_dir = repo_root.join(".synapse").join("worktrees").join(run_id);
        let mut removed = Vec::new();

        if run_worktrees_dir.exists() {
            if let Ok(entries) = fs::read_dir(&run_worktrees_dir) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.is_dir() {
                        // Safety check: ensure path is strictly inside .synapse/worktrees/<run_id>/
                        let norm_path = path.to_string_lossy().replace('\\', "/");
                        let norm_expected = run_worktrees_dir.to_string_lossy().replace('\\', "/");
                        if norm_path.starts_with(&norm_expected) {
                            let _ = git_mgr.remove_worktree(&path).await;
                            removed.push(path);
                        } else {
                            warn!("Skipping unsafe path removal: {:?}", path);
                        }
                    }
                }
            }
            let _ = fs::remove_dir_all(&run_worktrees_dir);
        }

        let _ = git_mgr.run_git(&["worktree", "prune"], None).await;

        if !keep_artifacts {
            let run_dir = repo_root.join(".synapse").join("runs").join(run_id);
            if run_dir.exists() {
                let _ = fs::remove_dir_all(run_dir);
            }
        }

        Ok(CleanupSummary {
            run_id: run_id.to_string(),
            removed_worktrees: removed,
            pruned_git_worktrees: true,
            artifacts_kept: keep_artifacts,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[tokio::test]
    async fn test_inspect_and_cancel_run() {
        let dir = tempdir().unwrap();
        let repo_root = dir.path();

        let _ = tokio::process::Command::new("git")
            .args(["init"])
            .current_dir(repo_root)
            .output()
            .await;

        let git = GitManager::new(repo_root).unwrap();
        git.run_git(&["init"], None).await.unwrap();
        git.run_git(&["config", "user.name", "Tester"], None)
            .await
            .unwrap();
        git.run_git(&["config", "user.email", "test@test.com"], None)
            .await
            .unwrap();

        std::fs::write(repo_root.join("README.md"), "test").unwrap();
        git.run_git(&["add", "README.md"], None).await.unwrap();
        git.run_git(&["commit", "-m", "init"], None).await.unwrap();

        let head = git.run_git(&["rev-parse", "HEAD"], None).await.unwrap();
        let head = head.trim().to_string();

        let store = RunStore::new(repo_root, "run-cancel-test").unwrap();
        store
            .initialize(Some(head.clone()), Some("hash123".into()))
            .unwrap();
        store
            .transition_to(
                RunState::Implementation,
                vec![],
                TransitionOutcome::Success,
                None,
            )
            .unwrap();

        let inspection = RecoveryManager::inspect_run(repo_root, "run-cancel-test")
            .await
            .unwrap();
        assert_eq!(inspection.state, RunState::Implementation);
        assert!(inspection.can_resume);
        assert!(!inspection.reconciliation_required);

        // Cancel
        RecoveryManager::cancel_run(repo_root, "run-cancel-test", Some("Stopped by user".into()))
            .await
            .unwrap();

        let after_cancel = RecoveryManager::inspect_run(repo_root, "run-cancel-test")
            .await
            .unwrap();
        assert_eq!(after_cancel.state, RunState::Cancelled);
        assert!(!after_cancel.can_resume);
    }
}
