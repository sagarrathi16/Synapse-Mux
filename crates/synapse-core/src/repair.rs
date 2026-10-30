use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::check::CheckResult;
use crate::manifest::SynapseSpec;
use crate::review::ReviewFinding;
use crate::security::{scrub_secrets, truncate_output};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FailedCheckSummary {
    pub name: String,
    pub command: Vec<String>,
    pub exit_code: Option<i32>,
    pub diagnostic: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepairContextBundle {
    pub run_id: String,
    pub iteration: usize,
    pub max_iterations: usize,
    pub worker_name: String,
    pub contract_hash: String,
    pub contract_files: Vec<String>,
    pub failing_worker_diff: String,
    pub failed_checks: Vec<FailedCheckSummary>,
    pub relevant_findings: Vec<ReviewFinding>,
    pub timestamp: DateTime<Utc>,
}

impl RepairContextBundle {
    /// Formats the bounded repair prompt string for the worker
    pub fn format_prompt(&self) -> String {
        let mut p = String::new();
        p.push_str(&format!(
            "# SYNAPSE REPAIR DIRECTIVE (Iteration {} of {})\n\n",
            self.iteration, self.max_iterations
        ));
        p.push_str(&format!(
            "Worker: {}\nContract SHA-256: {}\n\n",
            self.worker_name, self.contract_hash
        ));
        p.push_str("Deterministic verification failed after merging into the staging worktree.\n");
        p.push_str("You must fix the implementation within your partition so that all verification checks pass.\n");
        p.push_str("DO NOT modify contract files or files outside your partition.\n\n");

        p.push_str("## 1. Failed Verification Checks\n");
        for fc in &self.failed_checks {
            p.push_str(&format!(
                "- Check: `{}`\n  Command: `{}`\n  Exit Code: {:?}\n  Diagnostic Output:\n```\n{}\n```\n\n",
                fc.name,
                fc.command.join(" "),
                fc.exit_code,
                fc.diagnostic
            ));
        }

        if !self.relevant_findings.is_empty() {
            p.push_str("## 2. Independent Review Findings\n");
            for rf in &self.relevant_findings {
                p.push_str(&format!(
                    "- [{:?}] {}{}: {}\n",
                    rf.severity,
                    rf.file_path
                        .as_deref()
                        .map(|f| format!("{f}: "))
                        .unwrap_or_default(),
                    rf.title,
                    rf.description
                ));
            }
            p.push('\n');
        }

        p.push_str("## 3. Your Current Diff\n```diff\n");
        p.push_str(&self.failing_worker_diff);
        p.push_str("\n```\n\n");
        p.push_str("Please resolve the failures now.");
        p
    }
}

pub struct ContextFirewall;

impl ContextFirewall {
    /// Assembles an isolated, bounded repair context bundle adhering to PRD FR-7.
    /// Scrubs secrets, enforces byte limits, and isolates to the target worker.
    #[allow(clippy::too_many_arguments)]
    pub fn assemble_repair_bundle(
        run_id: &str,
        iteration: usize,
        max_iterations: usize,
        worker_name: &str,
        contract_hash: &str,
        spec: &SynapseSpec,
        worker_diff: &str,
        failed_checks: &[CheckResult],
        findings: &[ReviewFinding],
        max_bytes: usize,
    ) -> RepairContextBundle {
        let scrubbed_diff = scrub_secrets(worker_diff);
        let bounded_diff = truncate_output(&scrubbed_diff, max_bytes / 2, true);

        let mut check_summaries = Vec::new();
        for fc in failed_checks {
            let combined_diag = if !fc.stderr.trim().is_empty() {
                fc.stderr.clone()
            } else {
                fc.stdout.clone()
            };

            let scrubbed_diag = scrub_secrets(&combined_diag);
            let bounded_diag = truncate_output(&scrubbed_diag, max_bytes / 4, true);

            check_summaries.push(FailedCheckSummary {
                name: fc.name.clone(),
                command: fc.command.clone(),
                exit_code: fc.exit_code,
                diagnostic: bounded_diag,
            });
        }

        let relevant_findings: Vec<ReviewFinding> = findings
            .iter()
            .filter(|rf| {
                if let Some(ref path) = rf.file_path
                    && let Some(partition) = spec.partitions.get(worker_name)
                {
                    return partition
                        .owned_globs
                        .iter()
                        .any(|g| SynapseSpec::path_matches_glob(path, g));
                }
                true
            })
            .cloned()
            .collect();

        RepairContextBundle {
            run_id: run_id.to_string(),
            iteration,
            max_iterations,
            worker_name: worker_name.to_string(),
            contract_hash: contract_hash.to_string(),
            contract_files: spec.contract_files.clone(),
            failing_worker_diff: bounded_diff,
            failed_checks: check_summaries,
            relevant_findings,
            timestamp: Utc::now(),
        }
    }

    /// Determines which worker partition is responsible for a given failed check or file
    pub fn determine_responsible_workers(
        failed_checks: &[CheckResult],
        spec: &SynapseSpec,
    ) -> Vec<String> {
        let mut responsible = Vec::new();

        for fc in failed_checks {
            let text = format!("{} {} {}", fc.name, fc.stdout, fc.stderr);
            for (worker, partition) in &spec.partitions {
                for glob in &partition.owned_globs {
                    let pattern = glob.trim_end_matches("/**").trim_end_matches("/*");
                    if text.contains(pattern) && !responsible.contains(worker) {
                        responsible.push(worker.clone());
                    }
                }
            }
        }

        // If responsibility could not be determined specifically, include all workers
        if responsible.is_empty() {
            responsible = spec.partitions.keys().cloned().collect();
        }

        responsible.sort();
        responsible
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::check::CheckStatus;

    #[test]
    fn test_context_firewall_assembly_and_scrubbing() {
        let spec_json = serde_json::json!({
            "schema_version": 1,
            "run_id": "repair-run-001",
            "feature_id": "repair-test",
            "base_commit": "abc1234",
            "contract_revision": 1,
            "contract_files": ["src/contracts/events.ts"],
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
                "context_bundle_max_bytes": 1024
            },
            "approval_policy": {
                "contract": "human_required",
                "destructive_commands": "human_required",
                "merge_to_base": "human_required"
            }
        });
        let spec: SynapseSpec = serde_json::from_value(spec_json).unwrap();

        let raw_diff = "diff --git a/src/api/index.ts b/src/api/index.ts\n+const token = 'sk-ant-secret123456789';";
        let failed_check = CheckResult {
            name: "typecheck".into(),
            command: vec!["tsc".into()],
            status: CheckStatus::Failed,
            exit_code: Some(2),
            stdout: String::new(),
            stderr: "Error in src/api/index.ts: secret sk-proj-1234567890123456789012 invalid"
                .into(),
            duration_ms: 100,
            required: true,
            message: None,
        };

        let bundle = ContextFirewall::assemble_repair_bundle(
            "repair-run-001",
            1,
            3,
            "claude",
            "contract_hash_123",
            &spec,
            raw_diff,
            std::slice::from_ref(&failed_check),
            &[],
            1024,
        );

        let prompt = bundle.format_prompt();
        assert!(!prompt.contains("sk-ant-secret123456789"));
        assert!(!prompt.contains("sk-proj-1234567890123456789012"));
        assert!(prompt.contains("sk-ant-***REDACTED***"));
        assert!(prompt.contains("sk-***REDACTED***"));
        assert!(prompt.contains("Iteration 1 of 3"));

        let workers = ContextFirewall::determine_responsible_workers(&[failed_check], &spec);
        assert_eq!(workers, vec!["claude"]);
    }
}
