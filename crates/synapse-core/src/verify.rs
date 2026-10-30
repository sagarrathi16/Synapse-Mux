use std::path::Path;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::check::{CheckError, CheckResult, CheckRunner, CheckStatus};
use crate::context_packer::ContextPacker;
use crate::manifest::SynapseSpec;
use crate::review::{FindingSeverity, ReviewFinding, Reviewer, execute_adversarial_tests};

pub type ExecutedCheckResult = CheckResult;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelFinding {
    pub reviewer: String,
    pub target_worker: String,
    pub severity: FindingSeverity,
    pub file_path: Option<String>,
    pub line_number: Option<u32>,
    pub title: String,
    pub description: String,
    pub suggested_fix: Option<String>,
}

impl From<(&str, &str, ReviewFinding)> for ModelFinding {
    fn from((reviewer, target, rf): (&str, &str, ReviewFinding)) -> Self {
        Self {
            reviewer: reviewer.to_string(),
            target_worker: target.to_string(),
            severity: rf.severity,
            file_path: rf.file_path,
            line_number: rf.line_number,
            title: rf.title,
            description: rf.description,
            suggested_fix: rf.suggested_fix,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PipelineVerificationReport {
    pub all_required_passed: bool,
    pub contract_hash_valid: bool,
    pub step_results: Vec<ExecutedCheckResult>,
    pub model_findings: Vec<ModelFinding>,
    pub adversarial_checks: Vec<ExecutedCheckResult>,
    pub diagnostics_summary: String,
}

pub struct OrderedVerificationPipeline;

impl OrderedVerificationPipeline {
    /// Executes the strict 7-step verification pipeline inside WT_STAGE adhering to PRD FR-6:
    /// 1. Manifest schema validation
    /// 2. Path ownership & contract_hash integrity verification
    /// 3. Formatting, linting, and static type checks
    /// 4. Hash-locked contract_tests and repo tests
    /// 5. Configured security/migration checks
    /// 6. Independent cross-model review & adversarial test generation
    /// 7. Execute any newly proposed adversarial tests inside WT_STAGE before counting as evidence
    #[allow(clippy::too_many_arguments)]
    pub async fn execute_pipeline(
        wt_stage: &Path,
        spec: &SynapseSpec,
        expected_contract_hash: Option<&str>,
        claude_diff: &str,
        codex_diff: &str,
        reviewer_claude: Option<Arc<dyn Reviewer>>,
        reviewer_codex: Option<Arc<dyn Reviewer>>,
        run_dir: &Path,
    ) -> Result<PipelineVerificationReport, CheckError> {
        let mut executed_checks = Vec::new();
        let mut model_findings = Vec::new();
        let mut adversarial_checks = Vec::new();

        // Step 1: Manifest Schema Validation
        let spec_file = wt_stage.join("synapse-spec.json");
        if spec_file.exists() {
            match SynapseSpec::from_file(&spec_file) {
                Ok(s) => match s.validate(wt_stage) {
                    Ok(()) => {
                        executed_checks.push(CheckResult {
                            name: "step_1_manifest_schema_validation".into(),
                            command: vec!["synapse".into(), "validate-spec".into()],
                            status: CheckStatus::Passed,
                            exit_code: Some(0),
                            stdout: "Manifest schema and partition boundaries valid".into(),
                            stderr: String::new(),
                            duration_ms: 0,
                            required: true,
                            message: None,
                        });
                    }
                    Err(e) => {
                        executed_checks.push(CheckResult {
                            name: "step_1_manifest_schema_validation".into(),
                            command: vec!["synapse".into(), "validate-spec".into()],
                            status: CheckStatus::Failed,
                            exit_code: Some(1),
                            stdout: String::new(),
                            stderr: format!("Manifest validation failed: {e}"),
                            duration_ms: 0,
                            required: true,
                            message: Some("Invalid synapse-spec.json".into()),
                        });
                    }
                },
                Err(e) => {
                    executed_checks.push(CheckResult {
                        name: "step_1_manifest_schema_validation".into(),
                        command: vec!["synapse".into(), "validate-spec".into()],
                        status: CheckStatus::Failed,
                        exit_code: Some(1),
                        stdout: String::new(),
                        stderr: format!("Manifest deserialization failed: {e}"),
                        duration_ms: 0,
                        required: true,
                        message: Some("Malformed synapse-spec.json".into()),
                    });
                }
            }
        }

        // Step 2: Contract Hash & Path Ownership Integrity
        let mut contract_hash_valid = true;
        if let Some(expected_hash) = expected_contract_hash {
            match spec.compute_contract_hash(wt_stage) {
                Ok(current_hash) => {
                    if current_hash != expected_hash {
                        contract_hash_valid = false;
                        executed_checks.push(CheckResult {
                            name: "step_2_contract_hash_integrity".into(),
                            command: vec!["synapse".into(), "verify-contract-hash".into()],
                            status: CheckStatus::Failed,
                            exit_code: Some(1),
                            stdout: String::new(),
                            stderr: format!(
                                "Contract files or tests mutated! Expected hash '{expected_hash}', got '{current_hash}'"
                            ),
                            duration_ms: 0,
                            required: true,
                            message: Some("Contract hash drift detected in staging".into()),
                        });
                    } else {
                        executed_checks.push(CheckResult {
                            name: "step_2_contract_hash_integrity".into(),
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
                    executed_checks.push(CheckResult {
                        name: "step_2_contract_hash_integrity".into(),
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

        // Steps 3, 4, 5: Execute verification commands (static checks, unit/integration tests, security checks)
        let command_results = CheckRunner::run_all(&spec.verification_commands, wt_stage).await;
        executed_checks.extend(command_results);

        // Step 6: Independent Cross-Model Review & Adversarial Test Proposals
        // PRD FR-6: Codex reviews Claude's partition diff; Claude reviews Codex's partition diff.
        let mut proposed_tests = Vec::new();

        // 6a. Codex reviews Claude's partition diff
        if let Some(ref rev_codex) = reviewer_codex {
            let _ = ContextPacker::pack_cross_review_payload(
                &spec.run_id,
                "codex",
                "openai",
                "claude",
                claude_diff,
                spec,
                expected_contract_hash.unwrap_or_default(),
                spec.limits.context_bundle_max_bytes,
                run_dir,
            );

            if let Ok(report) = rev_codex
                .perform_review(claude_diff, &spec.contract_files, spec, wt_stage)
                .await
            {
                for f in report.findings {
                    model_findings.push(ModelFinding {
                        reviewer: "codex".into(),
                        target_worker: "claude".into(),
                        severity: f.severity,
                        file_path: f.file_path,
                        line_number: f.line_number,
                        title: f.title,
                        description: f.description,
                        suggested_fix: f.suggested_fix,
                    });
                }
                proposed_tests.extend(report.proposed_tests);
            }
        }

        // 6b. Claude reviews Codex's partition diff
        if let Some(ref rev_claude) = reviewer_claude {
            let _ = ContextPacker::pack_cross_review_payload(
                &spec.run_id,
                "claude",
                "anthropic",
                "codex",
                codex_diff,
                spec,
                expected_contract_hash.unwrap_or_default(),
                spec.limits.context_bundle_max_bytes,
                run_dir,
            );

            if let Ok(report) = rev_claude
                .perform_review(codex_diff, &spec.contract_files, spec, wt_stage)
                .await
            {
                for f in report.findings {
                    model_findings.push(ModelFinding {
                        reviewer: "claude".into(),
                        target_worker: "codex".into(),
                        severity: f.severity,
                        file_path: f.file_path,
                        line_number: f.line_number,
                        title: f.title,
                        description: f.description,
                        suggested_fix: f.suggested_fix,
                    });
                }
                proposed_tests.extend(report.proposed_tests);
            }
        }

        // Step 7: Execute proposed adversarial tests inside WT_STAGE
        // PRD Invariant: execute any newly proposed tests before counting as evidence.
        if !proposed_tests.is_empty()
            && let Ok(results) = execute_adversarial_tests(wt_stage, &proposed_tests).await
        {
            adversarial_checks = results;
        }

        let all_required_checks_passed = contract_hash_valid
            && executed_checks
                .iter()
                .filter(|c| c.required)
                .all(|c| c.status == CheckStatus::Passed);

        let all_adversarial_passed = adversarial_checks
            .iter()
            .filter(|c| c.required)
            .all(|c| c.status == CheckStatus::Passed);

        let overall_passed = all_required_checks_passed && all_adversarial_passed;

        let mut diag = String::new();
        if overall_passed {
            diag.push_str("All ordered verification pipeline steps passed successfully.");
        } else {
            diag.push_str("Ordered verification pipeline failed:\n");
            for fc in executed_checks
                .iter()
                .filter(|c| c.status != CheckStatus::Passed)
            {
                diag.push_str(&format!(
                    " - [{}] {}: exit={:?}\n   Error: {}\n",
                    if fc.required { "REQUIRED" } else { "OPTIONAL" },
                    fc.name,
                    fc.exit_code,
                    fc.stderr.trim()
                ));
            }
            for ac in adversarial_checks
                .iter()
                .filter(|c| c.status != CheckStatus::Passed)
            {
                diag.push_str(&format!(
                    " - [ADVERSARIAL] {}: exit={:?}\n   Error: {}\n",
                    ac.name,
                    ac.exit_code,
                    ac.stderr.trim()
                ));
            }
        }

        Ok(PipelineVerificationReport {
            all_required_passed: overall_passed,
            contract_hash_valid,
            step_results: executed_checks,
            model_findings,
            adversarial_checks,
            diagnostics_summary: diag,
        })
    }
}
