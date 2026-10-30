use std::fs;
use std::path::Path;
use std::time::Instant;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::check::{CheckResult, CheckRunner, CheckStatus};
use crate::config::VerificationCommand;
use crate::manifest::SynapseSpec;
use crate::security::{scrub_secrets, validate_path_in_root};

#[derive(Error, Debug)]
pub enum ReviewError {
    #[error("Reviewer execution error: {0}")]
    Execution(String),
    #[error("I/O error during review: {0}")]
    Io(#[from] std::io::Error),
    #[error("Security error during review: {0}")]
    Security(#[from] crate::security::SecurityError),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum FindingSeverity {
    Nit,
    Minor,
    Major,
    Critical,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewFinding {
    pub severity: FindingSeverity,
    pub file_path: Option<String>,
    pub line_number: Option<u32>,
    pub title: String,
    pub description: String,
    pub suggested_fix: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdversarialTestProposal {
    pub test_name: String,
    pub relative_path: String,
    pub test_code: String,
    pub verification_command: VerificationCommand,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewReport {
    pub reviewer_name: String,
    pub findings: Vec<ReviewFinding>,
    pub proposed_tests: Vec<AdversarialTestProposal>,
    pub executed_adversarial_results: Vec<CheckResult>,
    pub review_duration_ms: u64,
}

#[async_trait]
pub trait Reviewer: Send + Sync {
    async fn perform_review(
        &self,
        staging_diff: &str,
        contract_files: &[String],
        spec: &SynapseSpec,
        worktree_dir: &Path,
    ) -> Result<ReviewReport, ReviewError>;
}

/// Executes adversarial test proposals in the staging worktree.
/// PRD Invariant: execute any newly generated tests before counting them as evidence.
pub async fn execute_adversarial_tests(
    worktree_dir: &Path,
    proposals: &[AdversarialTestProposal],
) -> Result<Vec<CheckResult>, ReviewError> {
    let mut results = Vec::new();

    for proposal in proposals {
        // Validate test target path is within worktree root
        let target_path = validate_path_in_root(&proposal.relative_path, worktree_dir)?;

        if let Some(parent) = target_path.parent() {
            fs::create_dir_all(parent)?;
        }

        // Write proposed test file
        fs::write(&target_path, &proposal.test_code)?;

        // Execute configured verification command
        match CheckRunner::run_command(&proposal.verification_command, worktree_dir).await {
            Ok(mut res) => {
                res.name = format!("adversarial:{}", proposal.test_name);
                results.push(res);
            }
            Err(e) => {
                results.push(CheckResult {
                    name: format!("adversarial:{}", proposal.test_name),
                    command: proposal.verification_command.argv.clone(),
                    status: CheckStatus::Failed,
                    exit_code: None,
                    stdout: String::new(),
                    stderr: scrub_secrets(&e.to_string()),
                    duration_ms: 0,
                    required: proposal.verification_command.required,
                    message: Some(format!("Failed to execute adversarial test: {e}")),
                });
            }
        }
    }

    Ok(results)
}

/// Mock Reviewer for deterministic testing
pub struct MockReviewer {
    name: String,
    findings: Vec<ReviewFinding>,
    proposals: Vec<AdversarialTestProposal>,
}

impl MockReviewer {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            findings: Vec::new(),
            proposals: Vec::new(),
        }
    }

    pub fn with_finding(mut self, finding: ReviewFinding) -> Self {
        self.findings.push(finding);
        self
    }

    pub fn with_proposal(mut self, proposal: AdversarialTestProposal) -> Self {
        self.proposals.push(proposal);
        self
    }
}

#[async_trait]
impl Reviewer for MockReviewer {
    async fn perform_review(
        &self,
        _staging_diff: &str,
        _contract_files: &[String],
        _spec: &SynapseSpec,
        worktree_dir: &Path,
    ) -> Result<ReviewReport, ReviewError> {
        let start = Instant::now();

        // Execute any proposed adversarial tests in the worktree
        let executed_results = execute_adversarial_tests(worktree_dir, &self.proposals).await?;

        Ok(ReviewReport {
            reviewer_name: self.name.clone(),
            findings: self.findings.clone(),
            proposed_tests: self.proposals.clone(),
            executed_adversarial_results: executed_results,
            review_duration_ms: start.elapsed().as_millis() as u64,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[tokio::test]
    async fn test_mock_reviewer_and_test_execution() {
        let dir = tempdir().unwrap();
        let wt = dir.path();

        let reviewer = MockReviewer::new("test-reviewer")
            .with_finding(ReviewFinding {
                severity: FindingSeverity::Major,
                file_path: Some("src/api/webhooks.ts".into()),
                line_number: Some(42),
                title: "Missing error handling for 500 status".into(),
                description: "The handler does not catch rejected promises".into(),
                suggested_fix: Some("Wrap in try/catch".into()),
            })
            .with_proposal(AdversarialTestProposal {
                test_name: "git_adversarial_check".into(),
                relative_path: "tests/adversarial/check.txt".into(),
                test_code: "adversarial probe content".into(),
                verification_command: VerificationCommand {
                    name: "adv_test".into(),
                    argv: vec!["git".into(), "--version".into()],
                    cwd: None,
                    timeout_seconds: 5,
                    required: true,
                },
            });

        let spec_json = serde_json::json!({
            "schema_version": 1,
            "run_id": "test-run",
            "feature_id": "test-feat",
            "base_commit": "abc123",
            "contract_revision": 1,
            "contract_files": [],
            "contract_tests": [],
            "partitions": {
                "claude": { "role": "api", "owned_globs": ["src/api/**"], "read_globs": [] },
                "codex": { "role": "worker", "owned_globs": ["src/worker/**"], "read_globs": [] }
            },
            "shared_file_policy": {},
            "shared_files": [],
            "verification_commands": [],
            "limits": {
                "max_parallel_workers": 2,
                "max_repair_iterations": 3,
                "worker_timeout_seconds": 60,
                "context_bundle_max_bytes": 262144
            },
            "approval_policy": {
                "contract": "human_required",
                "destructive_commands": "human_required",
                "merge_to_base": "human_required"
            }
        });
        let spec: SynapseSpec = serde_json::from_value(spec_json).unwrap();

        let report = reviewer
            .perform_review("diff content", &[], &spec, wt)
            .await
            .unwrap();

        assert_eq!(report.findings.len(), 1);
        assert_eq!(report.findings[0].severity, FindingSeverity::Major);
        assert_eq!(report.executed_adversarial_results.len(), 1);
        assert_eq!(
            report.executed_adversarial_results[0].status,
            CheckStatus::Passed
        );

        // Verify proposed file was written to disk
        assert!(wt.join("tests/adversarial/check.txt").exists());
    }
}
