use std::fs;
use std::path::Path;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvalArm {
    pub id: String,
    pub name: String,
    pub description: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StudyDesign {
    pub task_count: usize,
    pub repeats_per_task: usize,
    pub randomize_order: bool,
    pub arms: Vec<EvalArm>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PrimaryThreshold {
    pub metric: String,
    pub comparison: String,
    pub target_improvement_percentage: f64,
    pub condition: String,
    pub secondary_gate: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PreRegistration {
    pub version: String,
    pub registered_at: String,
    pub study_design: StudyDesign,
    pub primary_threshold: PrimaryThreshold,
    pub metrics: Vec<String>,
}

impl PreRegistration {
    pub fn load_from_file(path: impl AsRef<Path>) -> anyhow::Result<Self> {
        let content = fs::read_to_string(path.as_ref())?;
        let res = serde_json::from_str(&content)?;
        Ok(res)
    }

    pub fn validate_arm_exists(&self, arm_id: &str) -> bool {
        self.study_design.arms.iter().any(|a| a.id == arm_id)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskRunResult {
    pub task_id: String,
    pub arm_id: String,
    pub repeat_index: usize,
    pub first_integration_build_pass: bool,
    pub acceptance_criteria_pass: bool,
    pub human_integration_minutes: f64,
    pub task_wall_clock_seconds: f64,
    pub post_integration_defects: usize,
    pub contract_violations_detected: usize,
    pub repair_attempts: usize,
    pub observed_tokens: u64,
    pub estimated_cost_usd: f64,
    pub timestamp: DateTime<Utc>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn test_load_preregistration() {
        let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let path = manifest_dir.join("../../eval/preregistration.json");
        let prereg =
            PreRegistration::load_from_file(&path).expect("Failed to load preregistration.json");
        assert_eq!(prereg.study_design.task_count, 20);
        assert_eq!(prereg.study_design.repeats_per_task, 3);
        assert_eq!(prereg.study_design.arms.len(), 5);
        assert!(prereg.validate_arm_exists("A"));
        assert!(prereg.validate_arm_exists("B"));
        assert!(prereg.validate_arm_exists("C"));
        assert!(prereg.validate_arm_exists("C-same"));
        assert!(prereg.validate_arm_exists("D"));
    }
}
