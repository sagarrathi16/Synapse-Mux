use std::path::{Path, PathBuf};
use std::process::Stdio;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::process::Command;
use uuid::Uuid;

#[derive(Error, Debug)]
pub enum GitError {
    #[error("Git command failed: {cmd}\nExit code: {exit_code:?}\nStderr: {stderr}")]
    CommandFailed {
        cmd: String,
        exit_code: Option<i32>,
        stderr: String,
    },
    #[error(
        "Working tree is dirty. Stash or commit your changes first, or pass --allow-dirty-with-snapshot.\nDirty files:\n{0}"
    )]
    DirtyWorkingTree(String),
    #[error("Path traversal detected: {0}")]
    PathTraversal(String),
    #[error("Not a git repository: {0}")]
    NotAGitRepo(PathBuf),
    #[error("Worktree path already exists: {0}")]
    WorktreeAlreadyExists(PathBuf),
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PreflightResult {
    pub git_version: String,
    pub repo_root: PathBuf,
    pub current_commit: String,
    pub is_clean: bool,
    pub stash_snapshot_sha: Option<String>,
    pub claude_version: Option<String>,
    pub codex_version: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckpointArtifact {
    pub checkpoint_id: String,
    pub run_id: String,
    pub worker_name: String,
    pub base_commit: String,
    pub contract_hash: Option<String>,
    pub timestamp: DateTime<Utc>,
    pub modified_files: Vec<String>,
    pub added_files: Vec<String>,
    pub deleted_files: Vec<String>,
    pub untracked_files: Vec<String>,
    pub unified_diff: String,
}

#[derive(Debug, Clone)]
pub struct GitManager {
    repo_root: PathBuf,
}

impl GitManager {
    pub fn new(repo_root: impl AsRef<Path>) -> Result<Self, GitError> {
        let repo_root = repo_root.as_ref().to_path_buf();
        let git_dir = repo_root.join(".git");
        if !git_dir.exists() {
            return Err(GitError::NotAGitRepo(repo_root));
        }

        // Ensure .synapse is added to .git/info/exclude so it never dirties user's working tree
        let exclude_dir = git_dir.join("info");
        let exclude_file = exclude_dir.join("exclude");
        if std::fs::create_dir_all(&exclude_dir).is_ok() {
            let content = std::fs::read_to_string(&exclude_file).unwrap_or_default();
            if !content.contains(".synapse")
                && let Ok(mut f) = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&exclude_file)
            {
                use std::io::Write;
                let _ = writeln!(f, "\n.synapse/");
            }
        }

        Ok(Self { repo_root })
    }

    pub fn repo_root(&self) -> &Path {
        &self.repo_root
    }

    pub async fn run_git(&self, args: &[&str], cwd: Option<&Path>) -> Result<String, GitError> {
        let working_dir = cwd.unwrap_or(&self.repo_root);
        let mut cmd = Command::new("git");
        cmd.args(args)
            .current_dir(working_dir)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let output = cmd.output().await.map_err(GitError::Io)?;

        if output.status.success() {
            Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
        } else {
            let stderr = String::from_utf8_lossy(&output.stderr).to_string();
            let cmd_str = format!("git {}", args.join(" "));
            Err(GitError::CommandFailed {
                cmd: cmd_str,
                exit_code: output.status.code(),
                stderr,
            })
        }
    }

    pub async fn get_git_version(&self) -> Result<String, GitError> {
        self.run_git(&["--version"], None).await
    }

    pub async fn get_head_commit(&self) -> Result<String, GitError> {
        self.run_git(&["rev-parse", "HEAD"], None).await
    }

    pub async fn check_clean_working_tree(&self) -> Result<(bool, String), GitError> {
        let status = self.run_git(&["status", "--porcelain"], None).await?;
        let filtered: Vec<&str> = status
            .lines()
            .filter(|line| {
                let trimmed = line.trim();
                !trimmed.ends_with(".synapse/") && !trimmed.contains(".synapse")
            })
            .collect();

        if filtered.is_empty() {
            Ok((true, String::new()))
        } else {
            Ok((false, filtered.join("\n")))
        }
    }

    pub async fn create_stash_snapshot(&self) -> Result<String, GitError> {
        let sha = self.run_git(&["stash", "create"], None).await?;
        if sha.is_empty() {
            self.get_head_commit().await
        } else {
            Ok(sha)
        }
    }

    pub async fn detect_cli_version(&self, cli_name: &str) -> Option<String> {
        let mut cmd = Command::new(cli_name);
        cmd.arg("--version")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        match cmd.output().await {
            Ok(output) if output.status.success() => {
                let ver = String::from_utf8_lossy(&output.stdout).trim().to_string();
                if ver.is_empty() { None } else { Some(ver) }
            }
            _ => None,
        }
    }

    pub async fn preflight(
        &self,
        allow_dirty_with_snapshot: bool,
    ) -> Result<PreflightResult, GitError> {
        let git_version = self.get_git_version().await?;
        let current_commit = self.get_head_commit().await?;
        let (is_clean, dirty_status) = self.check_clean_working_tree().await?;

        let mut stash_snapshot_sha = None;
        if !is_clean {
            if allow_dirty_with_snapshot {
                let stash_sha = self.create_stash_snapshot().await?;
                stash_snapshot_sha = Some(stash_sha);
            } else {
                return Err(GitError::DirtyWorkingTree(dirty_status));
            }
        }

        let claude_version = self.detect_cli_version("claude").await;
        let codex_version = self.detect_cli_version("codex").await;

        Ok(PreflightResult {
            git_version,
            repo_root: self.repo_root.clone(),
            current_commit,
            is_clean,
            stash_snapshot_sha,
            claude_version,
            codex_version,
        })
    }

    pub fn worktree_base_dir(&self, run_id: &str) -> PathBuf {
        self.repo_root
            .join(".synapse")
            .join("worktrees")
            .join(run_id)
    }

    pub async fn create_worktree(
        &self,
        run_id: &str,
        worker_name: &str,
        base_commit: &str,
    ) -> Result<PathBuf, GitError> {
        let worktree_dir = self.worktree_base_dir(run_id).join(worker_name);
        if worktree_dir.exists() {
            return Err(GitError::WorktreeAlreadyExists(worktree_dir));
        }

        if let Some(parent) = worktree_dir.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(GitError::Io)?;
        }

        let worktree_str = worktree_dir.to_string_lossy().to_string();
        self.run_git(
            &["worktree", "add", "--detach", &worktree_str, base_commit],
            None,
        )
        .await?;

        Ok(worktree_dir)
    }

    pub async fn remove_worktree(&self, worktree_dir: &Path) -> Result<(), GitError> {
        if !worktree_dir.exists() {
            return Ok(());
        }

        let worktree_str = worktree_dir.to_string_lossy().to_string();
        // Force remove worktree from git registration
        let _ = self
            .run_git(&["worktree", "remove", "--force", &worktree_str], None)
            .await;

        // Ensure directory is deleted from disk
        if worktree_dir.exists() {
            let _ = tokio::fs::remove_dir_all(worktree_dir).await;
        }

        // Run git worktree prune
        let _ = self.run_git(&["worktree", "prune"], None).await;

        Ok(())
    }

    pub async fn capture_checkpoint(
        &self,
        worktree_dir: &Path,
        run_id: &str,
        worker_name: &str,
        base_commit: &str,
        contract_hash: Option<&str>,
    ) -> Result<CheckpointArtifact, GitError> {
        let status_out = self
            .run_git(&["status", "--porcelain"], Some(worktree_dir))
            .await?;

        let mut modified_files = Vec::new();
        let mut added_files = Vec::new();
        let mut deleted_files = Vec::new();
        let mut untracked_files = Vec::new();

        for line in status_out.lines() {
            if line.len() < 3 {
                continue;
            }
            let index_status = &line[0..1];
            let worktree_status = &line[1..2];
            let filename = line[2..].trim().to_string();

            if index_status == "?" || worktree_status == "?" {
                untracked_files.push(filename);
            } else if index_status == "M" || worktree_status == "M" {
                modified_files.push(filename);
            } else if index_status == "A" || worktree_status == "A" {
                added_files.push(filename);
            } else if index_status == "D" || worktree_status == "D" {
                deleted_files.push(filename);
            }
        }

        // Add untracked files to intent-to-add so git diff includes them
        if !untracked_files.is_empty() {
            let _ = self.run_git(&["add", "-N", "."], Some(worktree_dir)).await;
        }

        // Capture unified diff against HEAD
        let diff_out = self
            .run_git(&["diff", "HEAD"], Some(worktree_dir))
            .await
            .unwrap_or_default();

        Ok(CheckpointArtifact {
            checkpoint_id: Uuid::new_v4().to_string(),
            run_id: run_id.to_string(),
            worker_name: worker_name.to_string(),
            base_commit: base_commit.to_string(),
            contract_hash: contract_hash.map(ToString::to_string),
            timestamp: Utc::now(),
            modified_files,
            added_files,
            deleted_files,
            untracked_files,
            unified_diff: diff_out,
        })
    }

    pub fn validate_path_safety(&self, rel_path: &str) -> Result<PathBuf, GitError> {
        let full = self.repo_root.join(rel_path);
        let canonical_root = self.repo_root.canonicalize().map_err(GitError::Io)?;

        if full.exists() {
            let canonical = full.canonicalize().map_err(GitError::Io)?;
            if !canonical.starts_with(&canonical_root) {
                return Err(GitError::PathTraversal(format!(
                    "Path '{rel_path}' resolves outside repo root"
                )));
            }
            Ok(canonical)
        } else {
            // Check normalized parent path
            let normalized = full;
            let check_target = if let Some(parent) = normalized.parent() {
                if parent.exists() {
                    parent.canonicalize().map_err(GitError::Io)?
                } else {
                    canonical_root.clone()
                }
            } else {
                canonical_root.clone()
            };

            if !check_target.starts_with(&canonical_root) {
                return Err(GitError::PathTraversal(format!(
                    "Path '{rel_path}' parent resolves outside repo root"
                )));
            }
            Ok(normalized)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    async fn init_git_repo(path: &Path) {
        let run = |args: &[&str]| {
            let mut cmd = std::process::Command::new("git");
            cmd.args(args).current_dir(path);
            let status = cmd.status().expect("git command failed");
            assert!(status.success());
        };

        run(&["init"]);
        run(&["config", "user.name", "Synapse Test"]);
        run(&["config", "user.email", "test@synapse.local"]);
        fs::write(path.join("README.md"), b"# Test Repo\n").unwrap();
        run(&["add", "README.md"]);
        run(&["commit", "-m", "Initial commit"]);
    }

    #[tokio::test]
    async fn test_git_preflight_and_worktree() {
        let temp = TempDir::new().unwrap();
        let repo_root = temp.path();
        init_git_repo(repo_root).await;

        let git_mgr = GitManager::new(repo_root).unwrap();

        // 1. Preflight on clean tree
        let preflight = git_mgr.preflight(false).await.unwrap();
        assert!(preflight.is_clean);
        assert!(!preflight.git_version.is_empty());

        // 2. Worktree creation
        let wt = git_mgr
            .create_worktree("run-1", "worker-a", &preflight.current_commit)
            .await
            .unwrap();
        assert!(wt.exists());
        assert!(wt.join("README.md").exists());

        // 3. Make change in worktree and capture checkpoint
        fs::write(wt.join("README.md"), b"# Updated in Worktree\n").unwrap();
        fs::write(wt.join("new_file.txt"), b"untracked\n").unwrap();

        let checkpoint = git_mgr
            .capture_checkpoint(&wt, "run-1", "worker-a", &preflight.current_commit, None)
            .await
            .unwrap();

        assert_eq!(checkpoint.modified_files, vec!["README.md"]);
        assert_eq!(checkpoint.untracked_files, vec!["new_file.txt"]);
        assert!(checkpoint.unified_diff.contains("+untracked"));
        assert!(checkpoint.unified_diff.contains("+# Updated in Worktree"));

        // 4. Verify original repo remains 100% clean and untouched
        let (original_clean, _) = git_mgr.check_clean_working_tree().await.unwrap();
        assert!(original_clean);
        let orig_readme = fs::read_to_string(repo_root.join("README.md")).unwrap();
        assert_eq!(orig_readme, "# Test Repo\n");

        // 5. Cleanup worktree
        git_mgr.remove_worktree(&wt).await.unwrap();
        assert!(!wt.exists());
    }
}
