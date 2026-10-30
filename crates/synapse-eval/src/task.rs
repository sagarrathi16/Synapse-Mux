use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use synapse_core::manifest::{
    LimitsConfig, PartitionConfig, SharedFileRule, SynapseSpec, VerificationCommand,
};

/// Definition of an evaluation benchmark task.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvalTask {
    pub task_id: String,
    pub name: String,
    pub description: String,
    pub feature_prompt: String,

    #[serde(default)]
    pub contract_files: Vec<String>,
    #[serde(default)]
    pub contract_tests: Vec<String>,
    #[serde(default)]
    pub partitions: BTreeMap<String, Vec<String>>,
    #[serde(default)]
    pub shared_file_policy: BTreeMap<String, SharedFileRule>,
    #[serde(default)]
    pub shared_files: Vec<String>,

    #[serde(default)]
    pub ci_gate_commands: Vec<VerificationCommand>,
    #[serde(default)]
    pub hidden_tests: Vec<VerificationCommand>,

    /// Baseline files to populate in the repository before any arm starts.
    #[serde(default)]
    pub baseline_files: BTreeMap<String, String>,

    /// Expected implementation for worker A (e.g. claude).
    #[serde(default)]
    pub worker_a_impl: BTreeMap<String, String>,

    /// Expected implementation for worker B (e.g. codex).
    #[serde(default)]
    pub worker_b_impl: BTreeMap<String, String>,

    /// Monolithic single-agent implementation (for Arm A and Arm D).
    #[serde(default)]
    pub single_agent_impl: BTreeMap<String, String>,

    /// Alternative conflicting worker implementation for Arm B to simulate
    /// uncoordinated parallel worktree merge friction.
    #[serde(default)]
    pub uncoordinated_worker_b_impl: BTreeMap<String, String>,
}

impl EvalTask {
    /// Loads a single task from a JSON file.
    pub fn load_from_file(path: impl AsRef<Path>) -> Result<Self> {
        let content = fs::read_to_string(path.as_ref())
            .with_context(|| format!("Failed to read task file at {:?}", path.as_ref()))?;
        let task: Self = serde_json::from_str(&content)
            .with_context(|| format!("Failed to parse task JSON at {:?}", path.as_ref()))?;
        Ok(task)
    }

    /// Loads all JSON tasks from a directory in lexicographical order.
    pub fn load_all_from_dir(dir: impl AsRef<Path>) -> Result<Vec<Self>> {
        let dir = dir.as_ref();
        if !dir.exists() {
            return Ok(Vec::new());
        }

        let mut tasks = Vec::new();
        let mut entries: Vec<PathBuf> = fs::read_dir(dir)?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("json"))
            .collect();

        entries.sort();

        for entry in entries {
            let task = Self::load_from_file(&entry)?;
            tasks.push(task);
        }

        Ok(tasks)
    }

    /// Converts this task to a `SynapseSpec` ready to be committed and approved.
    pub fn to_synapse_spec(&self, base_commit: &str, run_id: &str) -> SynapseSpec {
        let mut partitions_map = BTreeMap::new();
        for (name, globs) in &self.partitions {
            partitions_map.insert(
                name.clone(),
                PartitionConfig {
                    role: name.clone(),
                    owned_globs: globs.clone(),
                    read_globs: vec![],
                },
            );
        }

        SynapseSpec {
            schema_version: 1,
            run_id: run_id.to_string(),
            feature_id: self.task_id.clone(),
            base_commit: base_commit.to_string(),
            contract_revision: 1,
            contract_files: self.contract_files.clone(),
            contract_tests: self.contract_tests.clone(),
            partitions: partitions_map,
            shared_file_policy: self.shared_file_policy.clone(),
            shared_files: self.shared_files.clone(),
            verification_commands: self.ci_gate_commands.clone(),
            limits: LimitsConfig {
                max_parallel_workers: 2,
                max_repair_iterations: 3,
                worker_timeout_seconds: 60,
                context_bundle_max_bytes: 262_144,
            },
            approval_policy: Default::default(),
        }
    }

    /// Populates baseline files into a repo working tree.
    pub fn populate_baseline(&self, repo_dir: &Path) -> Result<()> {
        for (rel_path, content) in &self.baseline_files {
            let full_path = repo_dir.join(rel_path);
            if let Some(parent) = full_path.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(&full_path, content)?;
        }
        Ok(())
    }
}
