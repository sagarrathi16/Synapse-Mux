use std::fs;
use std::path::Path;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::check::CheckResult;
use crate::manifest::SynapseSpec;
use crate::repair::{FailedCheckSummary, RepairContextBundle};
use crate::review::ReviewFinding;
use crate::security::{scrub_secrets, truncate_output};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditPayloadManifest {
    pub timestamp: DateTime<Utc>,
    pub run_id: String,
    pub step: String,
    pub provider: String,
    pub worker_name: String,
    pub included_file_paths: Vec<String>,
    pub payload_byte_count: usize,
    pub contract_hash: String,
    pub was_truncated: bool,
}

pub struct ContextPacker;

impl ContextPacker {
    /// Assembles a bounded, structured repair context bundle and writes an audit payload manifest.
    /// Strictly adheres to PRD FR-7: excludes raw JSONL logs and passing tests.
    #[allow(clippy::too_many_arguments)]
    pub fn pack_repair_bundle(
        run_id: &str,
        iteration: usize,
        max_iterations: usize,
        worker_name: &str,
        provider: &str,
        contract_hash: &str,
        spec: &SynapseSpec,
        worker_diff: &str,
        failed_checks: &[CheckResult],
        peer_findings: &[ReviewFinding],
        max_bytes: usize,
        run_dir: &Path,
    ) -> Result<RepairContextBundle, std::io::Error> {
        let scrubbed_diff = scrub_secrets(worker_diff);
        let was_truncated = scrubbed_diff.len() > max_bytes / 2;
        let bounded_diff = truncate_output(&scrubbed_diff, max_bytes / 2, true);

        let mut check_summaries = Vec::new();
        let mut included_file_paths = spec.contract_files.clone();

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

        // Only include review findings relevant to this worker's partition
        let relevant_findings: Vec<ReviewFinding> = peer_findings
            .iter()
            .filter(|rf| {
                if let Some(ref path) = rf.file_path
                    && let Some(partition) = spec.partitions.get(worker_name)
                {
                    included_file_paths.push(path.clone());
                    return partition
                        .owned_globs
                        .iter()
                        .any(|g| SynapseSpec::path_matches_glob(path, g));
                }
                true
            })
            .cloned()
            .collect();

        let bundle = RepairContextBundle {
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
        };

        // Write audit manifest to .synapse/runs/<run_id>/audit/sent_payload_<step>.json
        let prompt_str = bundle.format_prompt();
        let payload_byte_count = prompt_str.len();

        let audit_dir = run_dir.join("audit");
        fs::create_dir_all(&audit_dir)?;

        let audit_manifest = AuditPayloadManifest {
            timestamp: Utc::now(),
            run_id: run_id.to_string(),
            step: format!("repair_iter_{iteration}"),
            provider: provider.to_string(),
            worker_name: worker_name.to_string(),
            included_file_paths,
            payload_byte_count,
            contract_hash: contract_hash.to_string(),
            was_truncated,
        };

        let audit_file = audit_dir.join(format!(
            "sent_payload_repair_iter_{}_{}.json",
            iteration, worker_name
        ));
        fs::write(audit_file, serde_json::to_string_pretty(&audit_manifest)?)?;

        Ok(bundle)
    }

    /// Assembles cross-model review prompt payload and records audit manifest
    #[allow(clippy::too_many_arguments)]
    pub fn pack_cross_review_payload(
        run_id: &str,
        reviewer_name: &str,
        reviewer_provider: &str,
        target_worker: &str,
        target_diff: &str,
        spec: &SynapseSpec,
        contract_hash: &str,
        max_bytes: usize,
        run_dir: &Path,
    ) -> Result<String, std::io::Error> {
        let scrubbed_diff = scrub_secrets(target_diff);
        let was_truncated = scrubbed_diff.len() > max_bytes / 2;
        let bounded_diff = truncate_output(&scrubbed_diff, max_bytes / 2, true);

        let mut prompt = String::new();
        prompt.push_str(&format!(
            "# SYNAPSE CROSS-MODEL ADVERSARIAL REVIEW\n\nReviewer: {reviewer_name} (Provider: {reviewer_provider})\n"
        ));
        prompt.push_str(&format!(
            "Target Partition: {target_worker}\nContract Hash: {contract_hash}\n\n"
        ));
        prompt.push_str(
            "Inspect the peer partition's diff against the approved contract interfaces.\n",
        );
        prompt
            .push_str("Identify defects, unhandled edge cases, and propose adversarial tests.\n\n");
        prompt.push_str("## Target Diff\n```diff\n");
        prompt.push_str(&bounded_diff);
        prompt.push_str("\n```\n");

        let audit_dir = run_dir.join("audit");
        fs::create_dir_all(&audit_dir)?;

        let mut included_paths = spec.contract_files.clone();
        if let Some(target_partition) = spec.partitions.get(target_worker) {
            included_paths.extend(target_partition.owned_globs.clone());
        }

        let audit_manifest = AuditPayloadManifest {
            timestamp: Utc::now(),
            run_id: run_id.to_string(),
            step: format!("cross_review_by_{reviewer_name}"),
            provider: reviewer_provider.to_string(),
            worker_name: reviewer_name.to_string(),
            included_file_paths: included_paths,
            payload_byte_count: prompt.len(),
            contract_hash: contract_hash.to_string(),
            was_truncated,
        };

        let audit_file = audit_dir.join(format!("sent_payload_review_{}.json", reviewer_name));
        fs::write(audit_file, serde_json::to_string_pretty(&audit_manifest)?)?;

        Ok(prompt)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::check::CheckStatus;
    use tempfile::tempdir;

    #[test]
    fn test_context_packer_audit_manifest() {
        let dir = tempdir().unwrap();
        let run_dir = dir.path();

        let spec_json = serde_json::json!({
            "schema_version": 1,
            "run_id": "audit-test-run",
            "feature_id": "audit-test",
            "base_commit": "abc1234",
            "contract_revision": 1,
            "contract_files": ["src/contracts/order.ts"],
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

        let failed_check = CheckResult {
            name: "typecheck".into(),
            command: vec!["tsc".into()],
            status: CheckStatus::Failed,
            exit_code: Some(1),
            stdout: String::new(),
            stderr: "Type error in src/api/handler.ts".into(),
            duration_ms: 50,
            required: true,
            message: None,
        };

        let bundle = ContextPacker::pack_repair_bundle(
            "audit-test-run",
            1,
            3,
            "claude",
            "anthropic",
            "hash123",
            &spec,
            "+const x = 1;",
            &[failed_check],
            &[],
            262144,
            run_dir,
        )
        .unwrap();

        assert_eq!(bundle.worker_name, "claude");

        // Verify audit manifest was written
        let audit_file = run_dir.join("audit/sent_payload_repair_iter_1_claude.json");
        assert!(audit_file.exists());
        let content = fs::read_to_string(audit_file).unwrap();
        let manifest: AuditPayloadManifest = serde_json::from_str(&content).unwrap();
        assert_eq!(manifest.worker_name, "claude");
        assert_eq!(manifest.provider, "anthropic");
        assert!(manifest.payload_byte_count > 0);
        assert!(!manifest.was_truncated);
    }
}
