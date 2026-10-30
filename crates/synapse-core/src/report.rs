use std::fs;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::adapters::ObservedUsage;
use crate::check::{CheckResult, CheckStatus};
use crate::review::ReviewFinding;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TelemetryReport {
    pub observed_usage: ObservedUsage,
    pub estimated_cost_usd: f64,
    pub billed_cost_usd: Option<f64>,
    pub pricing_table_version: String,
    pub usage_source: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvidenceReport {
    pub run_id: String,
    pub feature_id: String,
    pub base_commit: String,
    pub contract_hash: Option<String>,
    pub status: String,
    pub started_at: DateTime<Utc>,
    pub completed_at: DateTime<Utc>,
    pub duration_seconds: f64,
    pub changed_files: Vec<String>,
    pub unified_diff: String,
    pub checks: Vec<CheckResult>,
    pub all_required_checks_passed: bool,
    #[serde(default)]
    pub review_findings: Vec<ReviewFinding>,
    #[serde(default)]
    pub executed_adversarial_checks: Vec<CheckResult>,
    #[serde(default)]
    pub repair_iterations_count: usize,
    pub telemetry: TelemetryReport,
    pub next_actions: Vec<String>,
}

impl EvidenceReport {
    pub fn to_json_string(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(self)
    }

    pub fn to_markdown_string(&self) -> String {
        let mut md = String::new();
        md.push_str("# Synapse-Mux Run Evidence Report\n\n");
        md.push_str(&format!("**Run ID:** `{}`  \n", self.run_id));
        md.push_str(&format!("**Feature:** `{}`  \n", self.feature_id));
        md.push_str(&format!("**Status:** `{}`  \n", self.status));
        md.push_str(&format!("**Base Commit:** `{}`  \n", self.base_commit));
        if let Some(ref h) = self.contract_hash {
            md.push_str(&format!("**Contract Hash:** `{}`  \n", h));
        }
        md.push_str(&format!(
            "**Duration:** {:.2}s  \n\n",
            self.duration_seconds
        ));

        md.push_str("## 1. Verification Checks\n\n");
        if self.checks.is_empty() {
            md.push_str("No verification checks configured.\n\n");
        } else {
            md.push_str("| Check | Required | Status | Duration | Exit Code |\n");
            md.push_str("| :--- | :---: | :---: | :---: | :---: |\n");
            for c in &self.checks {
                let status_icon = match c.status {
                    CheckStatus::Passed => "PASSED",
                    CheckStatus::Failed => "FAILED",
                    CheckStatus::NotRun => "NOT RUN",
                    CheckStatus::Inconclusive => "INCONCLUSIVE",
                };
                let req_str = if c.required { "Yes" } else { "No" };
                let exit_str = c
                    .exit_code
                    .map(|code| code.to_string())
                    .unwrap_or_else(|| "-".into());
                md.push_str(&format!(
                    "| `{}` | {} | **{}** | {}ms | {} |\n",
                    c.name, req_str, status_icon, c.duration_ms, exit_str
                ));
            }
            md.push('\n');
        }

        if !self.review_findings.is_empty() || !self.executed_adversarial_checks.is_empty() {
            md.push_str("## 2. Independent Model Review & Adversarial Tests\n\n");
            if !self.review_findings.is_empty() {
                md.push_str("### Findings\n\n");
                for f in &self.review_findings {
                    md.push_str(&format!(
                        "- **[{:?}]** {}{}: {}\n",
                        f.severity,
                        f.file_path
                            .as_deref()
                            .map(|p| format!("`{p}`: "))
                            .unwrap_or_default(),
                        f.title,
                        f.description
                    ));
                    if let Some(ref fix) = f.suggested_fix {
                        md.push_str(&format!("  *Suggested Fix:* {}\n", fix));
                    }
                }
                md.push('\n');
            }

            if !self.executed_adversarial_checks.is_empty() {
                md.push_str("### Executed Adversarial Tests\n\n");
                md.push_str("| Test | Status | Duration |\n");
                md.push_str("| :--- | :---: | :---: |\n");
                for t in &self.executed_adversarial_checks {
                    md.push_str(&format!(
                        "| `{}` | **{:?}** | {}ms |\n",
                        t.name, t.status, t.duration_ms
                    ));
                }
                md.push('\n');
            }
        }

        if self.repair_iterations_count > 0 {
            md.push_str(&format!(
                "## Repair Loop\n- Total repair iterations executed: {}\n\n",
                self.repair_iterations_count
            ));
        }

        md.push_str("## 3. Telemetry & Cost Honesty\n\n");
        md.push_str("> [!IMPORTANT]\n");
        md.push_str("> Cost estimates are calculated locally from versioned pricing tables. Billed cost is only populated if authoritatively returned by the provider.\n\n");
        md.push_str(&format!(
            "- **Observed Input Tokens:** {}\n",
            self.telemetry.observed_usage.input_tokens
        ));
        md.push_str(&format!(
            "- **Observed Output Tokens:** {}\n",
            self.telemetry.observed_usage.output_tokens
        ));
        if let Some(cached) = self.telemetry.observed_usage.cache_read_input_tokens {
            md.push_str(&format!("- **Observed Cache Read Tokens:** {}\n", cached));
        }
        md.push_str(&format!(
            "- **Estimated Cost:** ${:.4} (Pricing table: `{}`)\n",
            self.telemetry.estimated_cost_usd, self.telemetry.pricing_table_version
        ));
        if let Some(billed) = self.telemetry.billed_cost_usd {
            md.push_str(&format!(
                "- **Billed Cost (Authoritative):** ${:.4}\n",
                billed
            ));
        } else {
            md.push_str("- **Billed Cost:** Not reported by provider CLI\n");
        }
        md.push('\n');

        md.push_str("## 3. Modified & Created Files\n\n");
        if self.changed_files.is_empty() {
            md.push_str("No files changed.\n\n");
        } else {
            for f in &self.changed_files {
                md.push_str(&format!("- `{}`\n", f));
            }
            md.push('\n');
        }

        md.push_str("## 4. Unified Diff\n\n");
        if self.unified_diff.is_empty() {
            md.push_str("*(Diff is empty)*\n\n");
        } else {
            md.push_str("```diff\n");
            md.push_str(&self.unified_diff);
            if !self.unified_diff.ends_with('\n') {
                md.push('\n');
            }
            md.push_str("```\n\n");
        }

        md.push_str("## 5. Next Actions\n\n");
        for act in &self.next_actions {
            md.push_str(&format!("1. {}\n", act));
        }

        md
    }

    pub fn save_to_dir(&self, dir: &Path) -> Result<(PathBuf, PathBuf), std::io::Error> {
        fs::create_dir_all(dir)?;
        let json_path = dir.join("report.json");
        let md_path = dir.join("report.md");

        fs::write(&json_path, self.to_json_string()?)?;
        fs::write(&md_path, self.to_markdown_string())?;

        Ok((json_path, md_path))
    }
}
