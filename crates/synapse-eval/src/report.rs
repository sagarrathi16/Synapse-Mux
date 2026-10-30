use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{PreRegistration, TaskRunResult};

/// Basic statistical summary of a numerical series.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SummaryStats {
    pub mean: f64,
    pub median: f64,
    pub std_dev: f64,
    pub min: f64,
    pub max: f64,
}

impl SummaryStats {
    pub fn compute(values: &[f64]) -> Self {
        if values.is_empty() {
            return Self {
                mean: 0.0,
                median: 0.0,
                std_dev: 0.0,
                min: 0.0,
                max: 0.0,
            };
        }

        let n = values.len() as f64;
        let sum: f64 = values.iter().sum();
        let mean = sum / n;

        let mut sorted = values.to_vec();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));

        let median = if values.len() % 2 == 1 {
            sorted[values.len() / 2]
        } else {
            let mid = values.len() / 2;
            (sorted[mid - 1] + sorted[mid]) / 2.0
        };

        let variance = if values.len() > 1 {
            let sq_diff_sum: f64 = values.iter().map(|v| (v - mean).powi(2)).sum();
            sq_diff_sum / (n - 1.0)
        } else {
            0.0
        };
        let std_dev = variance.sqrt();

        Self {
            mean,
            median,
            std_dev,
            min: sorted.first().copied().unwrap_or(0.0),
            max: sorted.last().copied().unwrap_or(0.0),
        }
    }
}

/// Aggregated performance summary for an evaluation arm.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArmSummary {
    pub arm_id: String,
    pub total_runs: usize,
    pub first_integration_pass_rate: f64,
    pub acceptance_criteria_pass_rate: f64,
    pub human_integration_minutes: SummaryStats,
    pub task_wall_clock_seconds: SummaryStats,
    pub post_integration_defects_total: usize,
    pub contract_violations_detected_total: usize,
    pub repair_attempts_total: usize,
    pub observed_tokens_total: u64,
    pub observed_tokens_mean: f64,
    pub estimated_cost_usd_total: f64,
    pub estimated_cost_usd_mean: f64,
}

/// Paired task run comparison between Synapse-Mux (Arm C) and Uncoordinated Baseline (Arm B).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PairedDifference {
    pub task_id: String,
    pub repeat_index: usize,
    pub human_minutes_b: f64,
    pub human_minutes_c: f64,
    pub delta_human_minutes: f64, // B - C
    pub acceptance_pass_b: bool,
    pub acceptance_pass_c: bool,
}

/// Evaluation of the pre-registered Go/No-Go threshold criteria.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GoNoGoGate {
    pub passed: bool,
    pub primary_metric: String,
    pub median_human_minutes_b: f64,
    pub median_human_minutes_c: f64,
    pub improvement_percentage: f64,
    pub target_percentage: f64,
    pub primary_gate_passed: bool,

    pub hidden_pass_rate_c: f64,
    pub hidden_pass_rate_b: f64,
    pub no_reduction_vs_b: bool,

    pub hidden_pass_rate_d: f64,
    pub secondary_gate_passed: bool,

    pub median_wall_clock_c: f64,
    pub median_wall_clock_d: f64,
    pub wall_clock_comparable_or_lower: bool,

    pub reasons: Vec<String>,
}

/// Complete benchmark evaluation report with paired analysis and go/no-go outcome.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvaluationReport {
    pub generated_at: DateTime<Utc>,
    pub preregistration_version: String,
    pub total_runs: usize,
    pub arms: BTreeMap<String, ArmSummary>,
    pub paired_differences: Vec<PairedDifference>,
    pub go_no_go: GoNoGoGate,
}

impl EvaluationReport {
    /// Builds the report from raw task run results and pre-registration specification.
    pub fn build(results: &[TaskRunResult], prereg: &PreRegistration) -> Self {
        let mut arm_results: BTreeMap<String, Vec<&TaskRunResult>> = BTreeMap::new();
        for r in results {
            arm_results.entry(r.arm_id.clone()).or_default().push(r);
        }

        let mut arms = BTreeMap::new();
        for (arm_id, runs) in arm_results {
            let total = runs.len();
            if total == 0 {
                continue;
            }

            let first_pass_count = runs
                .iter()
                .filter(|r| r.first_integration_build_pass)
                .count();
            let accept_count = runs.iter().filter(|r| r.acceptance_criteria_pass).count();

            let human_times: Vec<f64> = runs.iter().map(|r| r.human_integration_minutes).collect();
            let wall_times: Vec<f64> = runs.iter().map(|r| r.task_wall_clock_seconds).collect();

            let defects: usize = runs.iter().map(|r| r.post_integration_defects).sum();
            let violations: usize = runs.iter().map(|r| r.contract_violations_detected).sum();
            let repairs: usize = runs.iter().map(|r| r.repair_attempts).sum();
            let tokens: u64 = runs.iter().map(|r| r.observed_tokens).sum();
            let cost: f64 = runs.iter().map(|r| r.estimated_cost_usd).sum();

            let summary = ArmSummary {
                arm_id: arm_id.clone(),
                total_runs: total,
                first_integration_pass_rate: (first_pass_count as f64 / total as f64) * 100.0,
                acceptance_criteria_pass_rate: (accept_count as f64 / total as f64) * 100.0,
                human_integration_minutes: SummaryStats::compute(&human_times),
                task_wall_clock_seconds: SummaryStats::compute(&wall_times),
                post_integration_defects_total: defects,
                contract_violations_detected_total: violations,
                repair_attempts_total: repairs,
                observed_tokens_total: tokens,
                observed_tokens_mean: tokens as f64 / total as f64,
                estimated_cost_usd_total: cost,
                estimated_cost_usd_mean: cost / total as f64,
            };

            arms.insert(arm_id, summary);
        }

        // Compute paired differences for Arm C vs Arm B
        let mut paired_differences = Vec::new();
        let b_runs: BTreeMap<(String, usize), &TaskRunResult> = results
            .iter()
            .filter(|r| r.arm_id == "B")
            .map(|r| ((r.task_id.clone(), r.repeat_index), r))
            .collect();

        for r_c in results.iter().filter(|r| r.arm_id == "C") {
            if let Some(r_b) = b_runs.get(&(r_c.task_id.clone(), r_c.repeat_index)) {
                paired_differences.push(PairedDifference {
                    task_id: r_c.task_id.clone(),
                    repeat_index: r_c.repeat_index,
                    human_minutes_b: r_b.human_integration_minutes,
                    human_minutes_c: r_c.human_integration_minutes,
                    delta_human_minutes: r_b.human_integration_minutes
                        - r_c.human_integration_minutes,
                    acceptance_pass_b: r_b.acceptance_criteria_pass,
                    acceptance_pass_c: r_c.acceptance_criteria_pass,
                });
            }
        }

        // Evaluate Go/No-Go Decision Gate
        let median_b = arms
            .get("B")
            .map(|a| a.human_integration_minutes.median)
            .unwrap_or(0.0);
        let median_c = arms
            .get("C")
            .map(|a| a.human_integration_minutes.median)
            .unwrap_or(0.0);

        let improvement_pct = if median_b > 0.0 {
            ((median_b - median_c) / median_b) * 100.0
        } else {
            0.0
        };

        let target_pct = prereg.primary_threshold.target_improvement_percentage;
        let primary_gate_passed = improvement_pct >= target_pct;

        let pass_rate_c = arms
            .get("C")
            .map(|a| a.acceptance_criteria_pass_rate)
            .unwrap_or(0.0);
        let pass_rate_b = arms
            .get("B")
            .map(|a| a.acceptance_criteria_pass_rate)
            .unwrap_or(0.0);
        let pass_rate_d = arms
            .get("D")
            .map(|a| a.acceptance_criteria_pass_rate)
            .unwrap_or(0.0);

        let no_reduction_vs_b = pass_rate_c >= pass_rate_b;
        let secondary_gate_passed = pass_rate_c >= pass_rate_d;

        let wall_c = arms
            .get("C")
            .map(|a| a.task_wall_clock_seconds.median)
            .unwrap_or(0.0);
        let wall_d = arms
            .get("D")
            .map(|a| a.task_wall_clock_seconds.median)
            .unwrap_or(0.0);
        let wall_clock_comparable_or_lower = wall_c <= (wall_d * 1.15);

        let mut reasons = Vec::new();
        if primary_gate_passed {
            reasons.push(format!(
                "Primary Gate Passed: Median human integration time reduced by {:.1}% (target >= {:.1}%)",
                improvement_pct, target_pct
            ));
        } else {
            reasons.push(format!(
                "Primary Gate Failed: Median human integration time reduced by only {:.1}% (target >= {:.1}%)",
                improvement_pct, target_pct
            ));
        }

        if no_reduction_vs_b {
            reasons.push(format!(
                "Quality Invariant Maintained: Arm C acceptance pass rate ({:.1}%) >= Baseline B ({:.1}%)",
                pass_rate_c, pass_rate_b
            ));
        } else {
            reasons.push(format!(
                "Quality Invariant Broken: Arm C acceptance pass rate ({:.1}%) < Baseline B ({:.1}%)",
                pass_rate_c, pass_rate_b
            ));
        }

        if secondary_gate_passed && wall_clock_comparable_or_lower {
            reasons.push(format!(
                "Secondary Gate Passed: Arm C acceptance pass rate ({:.1}%) >= Arm D ({:.1}%) with lower wall-clock time ({:.1}s vs {:.1}s)",
                pass_rate_c, pass_rate_d, wall_c, wall_d
            ));
        } else if !secondary_gate_passed {
            reasons.push(format!(
                "Secondary Gate Warning: Arm C acceptance pass rate ({:.1}%) < Arm D ({:.1}%)",
                pass_rate_c, pass_rate_d
            ));
        }

        let overall_passed = primary_gate_passed && no_reduction_vs_b;

        let go_no_go = GoNoGoGate {
            passed: overall_passed,
            primary_metric: prereg.primary_threshold.metric.clone(),
            median_human_minutes_b: median_b,
            median_human_minutes_c: median_c,
            improvement_percentage: improvement_pct,
            target_percentage: target_pct,
            primary_gate_passed,
            hidden_pass_rate_c: pass_rate_c,
            hidden_pass_rate_b: pass_rate_b,
            no_reduction_vs_b,
            hidden_pass_rate_d: pass_rate_d,
            secondary_gate_passed,
            median_wall_clock_c: wall_c,
            median_wall_clock_d: wall_d,
            wall_clock_comparable_or_lower,
            reasons,
        };

        Self {
            generated_at: Utc::now(),
            preregistration_version: prereg.version.clone(),
            total_runs: results.len(),
            arms,
            paired_differences,
            go_no_go,
        }
    }

    /// Renders the complete statistical report as GitHub-flavored Markdown.
    pub fn to_markdown(&self) -> String {
        let mut md = String::new();
        md.push_str("# Synapse-Mux: 5-Arm Comparative Benchmark Report\n\n");

        md.push_str(&format!(
            "> [!NOTE]\n> Generated at: `{}` | Pre-Registration Version: `{}` | Total Runs: `{}`\n\n",
            self.generated_at.to_rfc3339(),
            self.preregistration_version,
            self.total_runs
        ));

        // Go/No-Go Decision Alert
        if self.go_no_go.passed {
            md.push_str("> [!IMPORTANT]\n> **PILOT VERDICT: GO (PASS)**\n>\n");
        } else {
            md.push_str("> [!WARNING]\n> **PILOT VERDICT: NO-GO (FAIL)**\n>\n");
        }
        for reason in &self.go_no_go.reasons {
            md.push_str(&format!("> - {}\n", reason));
        }
        md.push('\n');

        // Primary Threshold LaTeX formula
        md.push_str("### 1. Primary Go/No-Go Decision Gate\n\n");
        md.push_str(
            "Evaluation formula for human integration time reduction relative to uncoordinated Baseline B:\n\n",
        );
        md.push_str("$$\\text{Improvement} = \\frac{\\text{median}(H_B) - \\text{median}(H_C)}{\\text{median}(H_B)} \\times 100\\%$$\n\n");
        md.push_str(&format!(
            "- **Baseline B Median Human Time:** `{:.2} min`\n",
            self.go_no_go.median_human_minutes_b
        ));
        md.push_str(&format!(
            "- **Synapse-Mux Arm C Median Human Time:** `{:.2} min`\n",
            self.go_no_go.median_human_minutes_c
        ));
        md.push_str(&format!(
            "- **Observed Improvement:** **`{:.1}%`** (Pre-registered threshold: `>= {:.1}%`)\n\n",
            self.go_no_go.improvement_percentage, self.go_no_go.target_percentage
        ));

        // Arm Comparison Table
        md.push_str("### 2. Multi-Arm Performance Summary\n\n");
        md.push_str("| Arm ID | Description | Runs | 1st Build Pass | Hidden Acceptance | Median Human (min) | Median Wall (s) | Cost ($) |\n");
        md.push_str("|:---|:---|:---:|:---:|:---:|:---:|:---:|:---:|\n");

        for arm_id in ["A", "B", "C", "C-same", "D"] {
            if let Some(arm) = self.arms.get(arm_id) {
                let name = match arm_id {
                    "A" => "Single Agent Baseline",
                    "B" => "Uncoordinated Worktrees",
                    "C" => "Synapse-Mux Heterogeneous",
                    "C-same" => "Synapse-Mux Homogeneous",
                    "D" => "Sequential Contract",
                    other => other,
                };
                md.push_str(&format!(
                    "| `{}` | {} | {} | {:.1}% | {:.1}% | {:.1} ± {:.1} | {:.1}s | ${:.2} |\n",
                    arm.arm_id,
                    name,
                    arm.total_runs,
                    arm.first_integration_pass_rate,
                    arm.acceptance_criteria_pass_rate,
                    arm.human_integration_minutes.median,
                    arm.human_integration_minutes.std_dev,
                    arm.task_wall_clock_seconds.median,
                    arm.estimated_cost_usd_total
                ));
            }
        }
        md.push('\n');

        // Paired Differences Section
        if !self.paired_differences.is_empty() {
            md.push_str("### 3. Paired Task Differences (Arm C vs Baseline B)\n\n");
            md.push_str("| Task ID | Repeat | Baseline B Time | Arm C Time | Time Saved (min) | B Pass | C Pass |\n");
            md.push_str("|:---|:---:|:---:|:---:|:---:|:---:|:---:|\n");
            for p in &self.paired_differences {
                md.push_str(&format!(
                    "| `{}` | #{} | {:.1} min | {:.1} min | **+{:.1} min** | {} | {} |\n",
                    p.task_id,
                    p.repeat_index + 1,
                    p.human_minutes_b,
                    p.human_minutes_c,
                    p.delta_human_minutes,
                    if p.acceptance_pass_b { "PASS" } else { "FAIL" },
                    if p.acceptance_pass_c { "PASS" } else { "FAIL" }
                ));
            }
            md.push('\n');
        }

        md
    }

    /// Writes `summary.md` and `summary.json` into the given output directory.
    pub fn save_to_dir(&self, output_dir: impl AsRef<Path>) -> Result<()> {
        let dir = output_dir.as_ref();
        fs::create_dir_all(dir)?;

        let md_path = dir.join("summary.md");
        fs::write(&md_path, self.to_markdown())?;

        let json_path = dir.join("summary.json");
        fs::write(&json_path, serde_json::to_string_pretty(self)?)?;

        Ok(())
    }
}
