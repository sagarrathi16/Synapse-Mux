use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::check::{CheckError, CheckResult, CheckRunner, CheckStatus};
use crate::manifest::SynapseSpec;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerificationReport {
    pub all_required_passed: bool,
    pub contract_hash_valid: bool,
    pub check_results: Vec<CheckResult>,
    pub failed_checks: Vec<CheckResult>,
    pub diagnostics_summary: String,
}

pub struct VerificationPipeline;

impl VerificationPipeline {
    /// Executes the full deterministic verification suite against a staging worktree
    pub async fn verify_staging(
        wt_stage: &Path,
        _repo_root: &Path,
        spec: &SynapseSpec,
        expected_contract_hash: Option<&str>,
    ) -> Result<VerificationReport, CheckError> {
        let mut check_results = Vec::new();

        // 1. Validate contract file immutability in staging worktree
        let mut contract_hash_valid = true;
        if let Some(expected_hash) = expected_contract_hash {
            match spec.compute_contract_hash(wt_stage) {
                Ok(current_hash) => {
                    if current_hash != expected_hash {
                        contract_hash_valid = false;
                        check_results.push(CheckResult {
                            name: "contract_hash_immutability".into(),
                            command: vec!["synapse".into(), "verify-contract-hash".into()],
                            status: CheckStatus::Failed,
                            exit_code: Some(1),
                            stdout: String::new(),
                            stderr: format!(
                                "Contract files or tests mutated in staging worktree! Expected hash '{expected_hash}', got '{current_hash}'"
                            ),
                            duration_ms: 0,
                            required: true,
                            message: Some("Contract hash drift detected in staging".into()),
                        });
                    } else {
                        check_results.push(CheckResult {
                            name: "contract_hash_immutability".into(),
                            command: vec!["synapse".into(), "verify-contract-hash".into()],
                            status: CheckStatus::Passed,
                            exit_code: Some(0),
                            stdout: format!("Contract hash verified: {current_hash}"),
                            stderr: String::new(),
                            duration_ms: 0,
                            required: true,
                            message: None,
                        });
                    }
                }
                Err(e) => {
                    contract_hash_valid = false;
                    check_results.push(CheckResult {
                        name: "contract_hash_immutability".into(),
                        command: vec!["synapse".into(), "verify-contract-hash".into()],
                        status: CheckStatus::Failed,
                        exit_code: Some(1),
                        stdout: String::new(),
                        stderr: format!("Failed to compute contract hash: {e}"),
                        duration_ms: 0,
                        required: true,
                        message: Some("Contract file error".into()),
                    });
                }
            }
        }

        // 2. Execute configured verification commands (typecheck, lint, tests)
        let command_results = CheckRunner::run_all(&spec.verification_commands, wt_stage).await;
        check_results.extend(command_results);

        let failed_checks: Vec<CheckResult> = check_results
            .iter()
            .filter(|c| c.status != CheckStatus::Passed)
            .cloned()
            .collect();

        let all_required_passed = contract_hash_valid
            && check_results
                .iter()
                .filter(|c| c.required)
                .all(|c| c.status == CheckStatus::Passed);

        let mut diag = String::new();
        if all_required_passed {
            diag.push_str("All required verification checks passed successfully.");
        } else {
            diag.push_str(&format!(
                "Verification failed: {} failed checks ({} required failures):\n",
                failed_checks.len(),
                failed_checks.iter().filter(|c| c.required).count()
            ));
            for fc in &failed_checks {
                diag.push_str(&format!(
                    " - [{}] {}: exit={:?}\n   Error: {}\n",
                    if fc.required { "REQUIRED" } else { "OPTIONAL" },
                    fc.name,
                    fc.exit_code,
                    fc.stderr.trim()
                ));
            }
        }

        Ok(VerificationReport {
            all_required_passed,
            contract_hash_valid,
            check_results,
            failed_checks,
            diagnostics_summary: diag,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[tokio::test]
    async fn test_verification_pipeline_contract_drift() {
        let dir = tempdir().unwrap();
        let wt = dir.path();

        let spec_json = serde_json::json!({
            "schema_version": 1,
            "run_id": "test-verify-run",
            "feature_id": "test-verify",
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
            "verification_commands": [
                {
                    "name": "pass_check",
                    "argv": ["git", "--version"],
                    "timeout_seconds": 5,
                    "required": true
                }
            ],
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
        let report = VerificationPipeline::verify_staging(
            wt,
            wt,
            &spec,
            Some("expected_hash_does_not_match"),
        )
        .await
        .unwrap();

        assert!(!report.all_required_passed);
        assert!(!report.contract_hash_valid);
        assert_eq!(report.failed_checks.len(), 1);
        assert_eq!(report.failed_checks[0].name, "contract_hash_immutability");
    }
}
