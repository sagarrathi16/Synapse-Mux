use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

#[derive(Error, Debug)]
pub enum ConfigError {
    #[error("IO error reading contract: {0}")]
    Io(#[from] std::io::Error),
    #[error("Serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
    #[error("Path traversal detected or path outside repository root: {0}")]
    PathOutsideRepo(String),
    #[error("Contract file does not exist: {0}")]
    MissingContractFile(PathBuf),
    #[error("Contract test file does not exist: {0}")]
    MissingContractTestFile(PathBuf),
    #[error("Overlapping owned paths between partition '{0}' and '{1}' for glob '{2}'")]
    OverlappingPartitions(String, String, String),
    #[error("Unsupported schema version: {0}")]
    UnsupportedSchemaVersion(u32),
    #[error("Validation error: {0}")]
    Validation(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PartitionConfig {
    pub role: String,
    pub owned_globs: Vec<String>,
    #[serde(default)]
    pub read_globs: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SharedFileRule {
    pub owner: String,
    #[serde(default)]
    pub resolution: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerificationCommand {
    pub name: String,
    pub argv: Vec<String>,
    #[serde(default = "default_cwd")]
    pub cwd: Option<String>,
    #[serde(default = "default_timeout")]
    pub timeout_seconds: u64,
    #[serde(default = "default_required")]
    pub required: bool,
}

fn default_cwd() -> Option<String> {
    None
}

fn default_timeout() -> u64 {
    300
}

fn default_required() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LimitsConfig {
    #[serde(default = "default_parallel_workers")]
    pub max_parallel_workers: usize,
    #[serde(default = "default_repair_iterations")]
    pub max_repair_iterations: usize,
    #[serde(default = "default_worker_timeout")]
    pub worker_timeout_seconds: u64,
    #[serde(default = "default_context_bytes")]
    pub context_bundle_max_bytes: usize,
}

impl Default for LimitsConfig {
    fn default() -> Self {
        Self {
            max_parallel_workers: 2,
            max_repair_iterations: 3,
            worker_timeout_seconds: 1800,
            context_bundle_max_bytes: 262144,
        }
    }
}

fn default_parallel_workers() -> usize {
    2
}
fn default_repair_iterations() -> usize {
    3
}
fn default_worker_timeout() -> u64 {
    1800
}
fn default_context_bytes() -> usize {
    262144
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ApprovalPolicyConfig {
    #[serde(default = "default_approval_required")]
    pub contract: String,
    #[serde(default = "default_approval_required")]
    pub destructive_commands: String,
    #[serde(default = "default_approval_required")]
    pub merge_to_base: String,
}

fn default_approval_required() -> String {
    "human_required".into()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SynapseSpec {
    pub schema_version: u32,
    pub run_id: String,
    pub feature_id: String,
    pub base_commit: String,
    #[serde(default = "default_contract_revision")]
    pub contract_revision: u32,
    #[serde(default)]
    pub contract_files: Vec<String>,
    #[serde(default)]
    pub contract_tests: Vec<String>,
    #[serde(default)]
    pub partitions: HashMap<String, PartitionConfig>,
    #[serde(default)]
    pub shared_file_policy: HashMap<String, SharedFileRule>,
    #[serde(default)]
    pub shared_files: Vec<String>,
    #[serde(default)]
    pub verification_commands: Vec<VerificationCommand>,
    #[serde(default)]
    pub limits: LimitsConfig,
    #[serde(default)]
    pub approval_policy: ApprovalPolicyConfig,
}

fn default_contract_revision() -> u32 {
    1
}

impl SynapseSpec {
    pub fn from_file(path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        let content = fs::read_to_string(path)?;
        let spec: Self = serde_json::from_str(&content)?;
        Ok(spec)
    }

    pub fn validate(&self, repo_root: &Path) -> Result<(), ConfigError> {
        if self.schema_version != 1 {
            return Err(ConfigError::UnsupportedSchemaVersion(self.schema_version));
        }

        let canonical_root = repo_root.canonicalize()?;

        // Verify contract files exist and are within repo
        for rel_path in &self.contract_files {
            let full_path = repo_root.join(rel_path);
            if !full_path.exists() {
                return Err(ConfigError::MissingContractFile(full_path));
            }
            let canonical = full_path.canonicalize()?;
            if !canonical.starts_with(&canonical_root) {
                return Err(ConfigError::PathOutsideRepo(rel_path.clone()));
            }
        }

        // Verify contract test files exist and are within repo
        for rel_path in &self.contract_tests {
            let full_path = repo_root.join(rel_path);
            if !full_path.exists() {
                return Err(ConfigError::MissingContractTestFile(full_path));
            }
            let canonical = full_path.canonicalize()?;
            if !canonical.starts_with(&canonical_root) {
                return Err(ConfigError::PathOutsideRepo(rel_path.clone()));
            }
        }

        // Verify partition ownership does not overlap
        let partition_names: Vec<&String> = self.partitions.keys().collect();
        for i in 0..partition_names.len() {
            for j in (i + 1)..partition_names.len() {
                let name_a = partition_names[i];
                let name_b = partition_names[j];
                let globs_a = &self.partitions[name_a].owned_globs;
                let globs_b = &self.partitions[name_b].owned_globs;

                for ga in globs_a {
                    for gb in globs_b {
                        if ga == gb {
                            return Err(ConfigError::OverlappingPartitions(
                                name_a.clone(),
                                name_b.clone(),
                                ga.clone(),
                            ));
                        }
                    }
                }
            }
        }

        Ok(())
    }

    pub fn compute_contract_hash(&self, repo_root: &Path) -> Result<String, ConfigError> {
        let mut hasher = Sha256::new();

        // 1. Hash the canonical spec JSON representation
        let spec_json = serde_json::to_string(self)?;
        hasher.update(spec_json.as_bytes());

        // 2. Hash contents of all contract_files in deterministic sorted order
        let mut sorted_contract_files = self.contract_files.clone();
        sorted_contract_files.sort();
        for rel in sorted_contract_files {
            hasher.update(rel.as_bytes());
            let full = repo_root.join(&rel);
            if full.exists() {
                let bytes = fs::read(&full)?;
                hasher.update(&bytes);
            }
        }

        // 3. Hash contents of all contract_tests in deterministic sorted order
        let mut sorted_contract_tests = self.contract_tests.clone();
        sorted_contract_tests.sort();
        for rel in sorted_contract_tests {
            hasher.update(rel.as_bytes());
            let full = repo_root.join(&rel);
            if full.exists() {
                let bytes = fs::read(&full)?;
                hasher.update(&bytes);
            }
        }

        Ok(hex::encode(hasher.finalize()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_spec_hash_and_validation() {
        let temp = TempDir::new().unwrap();
        let repo_root = temp.path();

        let contract_dir = repo_root.join("contracts");
        fs::create_dir_all(&contract_dir).unwrap();
        let contract_file = contract_dir.join("interface.ts");
        fs::write(&contract_file, b"export interface Service {}").unwrap();

        let tests_dir = repo_root.join("tests");
        fs::create_dir_all(&tests_dir).unwrap();
        let test_file = tests_dir.join("service.test.ts");
        fs::write(&test_file, b"test('works', () => {});").unwrap();

        let spec = SynapseSpec {
            schema_version: 1,
            run_id: "test-run".into(),
            feature_id: "test-feat".into(),
            base_commit: "abcdef".into(),
            contract_revision: 1,
            contract_files: vec!["contracts/interface.ts".into()],
            contract_tests: vec!["tests/service.test.ts".into()],
            partitions: HashMap::new(),
            shared_file_policy: HashMap::new(),
            shared_files: vec![],
            verification_commands: vec![],
            limits: LimitsConfig::default(),
            approval_policy: ApprovalPolicyConfig::default(),
        };

        spec.validate(repo_root).unwrap();

        let hash1 = spec.compute_contract_hash(repo_root).unwrap();
        assert!(!hash1.is_empty());

        // Modify file content -> hash must change!
        fs::write(&contract_file, b"export interface Service { id: string; }").unwrap();
        let hash2 = spec.compute_contract_hash(repo_root).unwrap();
        assert_ne!(hash1, hash2);
    }
}
