use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

#[derive(Error, Debug)]
pub enum ManifestError {
    #[error("IO error reading manifest: {0}")]
    Io(#[from] std::io::Error),
    #[error("Serialization / schema error: {0}")]
    Serialization(#[from] serde_json::Error),
    #[error("Unsupported schema version: {0} (only version 1 is supported)")]
    UnsupportedSchemaVersion(u32),
    #[error("Path traversal detected or path outside repository root: '{0}'")]
    PathOutsideRepo(String),
    #[error("Contract file does not exist: {0}")]
    MissingContractFile(PathBuf),
    #[error("Contract test file does not exist: {0}")]
    MissingContractTestFile(PathBuf),
    #[error(
        "Overlapping partition write ownership between '{0}' and '{1}' for glob '{2}' and '{3}'"
    )]
    OverlappingPartitions(String, String, String, String),
    #[error("Shared file '{0}' is missing a policy rule in shared_file_policy")]
    MissingSharedFilePolicy(String),
    #[error("Shared file '{0}' has invalid owner '{1}' (must be 'supervisor' or a partition name)")]
    InvalidSharedFileOwner(String, String),
    #[error("Lockfile '{0}' must specify 'resolution': 'regenerate' in shared_file_policy")]
    LockfileMissingRegenerate(String),
    #[error("Validation failed: {0}")]
    Validation(String),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PartitionConfig {
    pub role: String,
    pub owned_globs: Vec<String>,
    #[serde(default)]
    pub read_globs: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SharedFileRule {
    pub owner: String,
    #[serde(default)]
    pub resolution: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct VerificationCommand {
    pub name: String,
    pub argv: Vec<String>,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default = "default_timeout")]
    pub timeout_seconds: u64,
    #[serde(default = "default_required")]
    pub required: bool,
}

fn default_timeout() -> u64 {
    300
}

fn default_required() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ApprovalPolicyConfig {
    #[serde(default = "default_approval_required")]
    pub contract: String,
    #[serde(default = "default_approval_required")]
    pub destructive_commands: String,
    #[serde(default = "default_approval_required")]
    pub merge_to_base: String,
}

impl Default for ApprovalPolicyConfig {
    fn default() -> Self {
        Self {
            contract: default_approval_required(),
            destructive_commands: default_approval_required(),
            merge_to_base: default_approval_required(),
        }
    }
}

fn default_approval_required() -> String {
    "human_required".into()
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
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
    pub partitions: BTreeMap<String, PartitionConfig>,
    #[serde(default)]
    pub shared_file_policy: BTreeMap<String, SharedFileRule>,
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
    pub fn from_file(path: impl AsRef<Path>) -> Result<Self, ManifestError> {
        let content = fs::read_to_string(path)?;
        let spec: Self = serde_json::from_str(&content)?;
        Ok(spec)
    }

    pub fn to_json_pretty(&self) -> Result<String, ManifestError> {
        Ok(serde_json::to_string_pretty(self)?)
    }

    pub fn validate(&self, repo_root: &Path) -> Result<(), ManifestError> {
        if self.schema_version != 1 {
            return Err(ManifestError::UnsupportedSchemaVersion(self.schema_version));
        }

        let canonical_root = repo_root.canonicalize().map_err(ManifestError::Io)?;

        // 1. Verify contract files exist and do not escape repo root
        for rel in &self.contract_files {
            Self::check_path_safety(repo_root, &canonical_root, rel, true)?;
        }

        // 2. Verify contract tests exist and do not escape repo root
        for rel in &self.contract_tests {
            Self::check_path_safety(repo_root, &canonical_root, rel, false)?;
        }

        // 3. Verify shared files are safe and covered by shared_file_policy
        for file in &self.shared_files {
            if !self.shared_file_policy.contains_key(file) {
                return Err(ManifestError::MissingSharedFilePolicy(file.clone()));
            }
        }

        for (file, rule) in &self.shared_file_policy {
            let valid_owner =
                rule.owner == "supervisor" || self.partitions.contains_key(&rule.owner);
            if !valid_owner {
                return Err(ManifestError::InvalidSharedFileOwner(
                    file.clone(),
                    rule.owner.clone(),
                ));
            }

            // Lockfiles MUST have resolution: "regenerate"
            if Self::is_lockfile(file) && rule.resolution.as_deref() != Some("regenerate") {
                return Err(ManifestError::LockfileMissingRegenerate(file.clone()));
            }
        }

        // 4. Verify partition ownership does not overlap
        let partition_names: Vec<&String> = self.partitions.keys().collect();
        for i in 0..partition_names.len() {
            for j in (i + 1)..partition_names.len() {
                let p1 = partition_names[i];
                let p2 = partition_names[j];
                let globs1 = &self.partitions[p1].owned_globs;
                let globs2 = &self.partitions[p2].owned_globs;

                for g1 in globs1 {
                    for g2 in globs2 {
                        if Self::globs_overlap(g1, g2) {
                            return Err(ManifestError::OverlappingPartitions(
                                p1.clone(),
                                p2.clone(),
                                g1.clone(),
                                g2.clone(),
                            ));
                        }
                    }
                }
            }
        }

        Ok(())
    }

    fn check_path_safety(
        repo_root: &Path,
        canonical_root: &Path,
        rel: &str,
        is_contract_file: bool,
    ) -> Result<(), ManifestError> {
        let normalized = rel.replace('\\', "/");
        if normalized.starts_with("../") || normalized.contains("/../") || normalized == ".." {
            return Err(ManifestError::PathOutsideRepo(rel.to_string()));
        }

        let full_path = repo_root.join(rel);
        if !full_path.exists() {
            return if is_contract_file {
                Err(ManifestError::MissingContractFile(full_path))
            } else {
                Err(ManifestError::MissingContractTestFile(full_path))
            };
        }

        let canonical = full_path.canonicalize().map_err(ManifestError::Io)?;
        if !canonical.starts_with(canonical_root) {
            return Err(ManifestError::PathOutsideRepo(rel.to_string()));
        }

        Ok(())
    }

    pub fn is_lockfile(path: &str) -> bool {
        let name = Path::new(path)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(path);

        matches!(
            name,
            "pnpm-lock.yaml"
                | "package-lock.json"
                | "Cargo.lock"
                | "yarn.lock"
                | "bun.lockb"
                | "composer.lock"
                | "Gemfile.lock"
        )
    }

    pub fn globs_overlap(glob_a: &str, glob_b: &str) -> bool {
        let a = glob_a.replace('\\', "/").trim_matches('/').to_string();
        let b = glob_b.replace('\\', "/").trim_matches('/').to_string();

        if a == b {
            return true;
        }

        // Clean wildcards to find directory roots
        let root_a = a
            .split("/**")
            .next()
            .unwrap_or(&a)
            .split("/*")
            .next()
            .unwrap_or(&a);

        let root_b = b
            .split("/**")
            .next()
            .unwrap_or(&b)
            .split("/*")
            .next()
            .unwrap_or(&b);

        if root_a == root_b {
            return true;
        }

        // Check if one root contains the other
        if a.contains("/**") && root_b.starts_with(&format!("{root_a}/")) {
            return true;
        }

        if b.contains("/**") && root_a.starts_with(&format!("{root_b}/")) {
            return true;
        }

        false
    }

    pub fn path_matches_glob(rel_path: &str, glob: &str) -> bool {
        let p = rel_path.replace('\\', "/").trim_matches('/').to_string();
        let g = glob.replace('\\', "/").trim_matches('/').to_string();

        if p == g {
            return true;
        }

        if let Some(prefix) = g.strip_suffix("/**") {
            let prefix = prefix.trim_matches('/');
            return p.starts_with(prefix)
                && (p.len() == prefix.len() || p.as_bytes()[prefix.len()] == b'/');
        }

        if let Some(prefix) = g.strip_suffix("/*") {
            let prefix = prefix.trim_matches('/');
            if p.starts_with(prefix)
                && (p.len() == prefix.len() || p.as_bytes()[prefix.len()] == b'/')
            {
                let remainder = &p[prefix.len()..].trim_start_matches('/');
                return !remainder.contains('/');
            }
        }

        false
    }

    pub fn compute_contract_hash(&self, repo_root: &Path) -> Result<String, ManifestError> {
        let mut hasher = Sha256::new();

        // 1. Hash the canonical spec JSON
        let spec_json = serde_json::to_string(self)?;
        hasher.update(spec_json.as_bytes());

        // 2. Hash contents of all contract_files in deterministic sorted order
        let mut sorted_contracts = self.contract_files.clone();
        sorted_contracts.sort();
        for rel in sorted_contracts {
            hasher.update(rel.as_bytes());
            let full = repo_root.join(&rel);
            if full.exists() {
                let bytes = fs::read(&full)?;
                let normalized = Self::normalize_crlf(&bytes);
                hasher.update(&normalized);
            }
        }

        // 3. Hash contents of all contract_tests in deterministic sorted order
        let mut sorted_tests = self.contract_tests.clone();
        sorted_tests.sort();
        for rel in sorted_tests {
            hasher.update(rel.as_bytes());
            let full = repo_root.join(&rel);
            if full.exists() {
                let bytes = fs::read(&full)?;
                let normalized = Self::normalize_crlf(&bytes);
                hasher.update(&normalized);
            }
        }

        Ok(hex::encode(hasher.finalize()))
    }

    /// Normalizes CRLF line endings to LF so contract hashes are platform- and checkout-independent
    pub fn normalize_crlf(bytes: &[u8]) -> Vec<u8> {
        let mut normalized = Vec::with_capacity(bytes.len());
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] == b'\r' && i + 1 < bytes.len() && bytes[i + 1] == b'\n' {
                normalized.push(b'\n');
                i += 2;
            } else {
                normalized.push(bytes[i]);
                i += 1;
            }
        }
        normalized
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_glob_overlap_detection() {
        assert!(SynapseSpec::globs_overlap(
            "src/api/**",
            "src/api/webhooks/**"
        ));
        assert!(SynapseSpec::globs_overlap(
            "src/api/webhooks/**",
            "src/api/**"
        ));
        assert!(SynapseSpec::globs_overlap(
            "src/shared/types.ts",
            "src/shared/types.ts"
        ));
        assert!(!SynapseSpec::globs_overlap("src/api/**", "src/workers/**"));
        assert!(!SynapseSpec::globs_overlap(
            "src/components/dlq/**",
            "src/db/repositories/**"
        ));
    }

    #[test]
    fn test_path_matches_glob() {
        assert!(SynapseSpec::path_matches_glob(
            "src/api/webhooks/route.ts",
            "src/api/webhooks/**"
        ));
        assert!(SynapseSpec::path_matches_glob(
            "src/api/webhooks/deep/nested/handler.ts",
            "src/api/webhooks/**"
        ));
        assert!(SynapseSpec::path_matches_glob(
            "src/db/repositories/repo.ts",
            "src/db/repositories/*"
        ));
        assert!(!SynapseSpec::path_matches_glob(
            "src/db/repositories/nested/repo.ts",
            "src/db/repositories/*"
        ));
    }

    #[test]
    fn test_deny_unknown_fields() {
        let json_with_extra = serde_json::json!({
            "schema_version": 1,
            "run_id": "r1",
            "feature_id": "f1",
            "base_commit": "abc",
            "unknown_injected_field": "disallowed"
        });

        let res: Result<SynapseSpec, _> = serde_json::from_value(json_with_extra);
        assert!(res.is_err(), "Should reject unknown fields");
    }

    #[test]
    fn test_lockfile_requires_regenerate() {
        let temp = TempDir::new().unwrap();
        let root = temp.path();

        let mut spec = SynapseSpec {
            schema_version: 1,
            run_id: "r1".into(),
            feature_id: "f1".into(),
            base_commit: "abc".into(),
            contract_revision: 1,
            contract_files: vec![],
            contract_tests: vec![],
            partitions: BTreeMap::new(),
            shared_file_policy: BTreeMap::new(),
            shared_files: vec!["pnpm-lock.yaml".into()],
            verification_commands: vec![],
            limits: LimitsConfig::default(),
            approval_policy: ApprovalPolicyConfig::default(),
        };

        // Missing from shared_file_policy
        assert!(spec.validate(root).is_err());

        // Policy without regenerate resolution
        spec.shared_file_policy.insert(
            "pnpm-lock.yaml".into(),
            SharedFileRule {
                owner: "supervisor".into(),
                resolution: None,
            },
        );
        assert!(spec.validate(root).is_err());

        // Policy with regenerate resolution
        spec.shared_file_policy.insert(
            "pnpm-lock.yaml".into(),
            SharedFileRule {
                owner: "supervisor".into(),
                resolution: Some("regenerate".into()),
            },
        );
        assert!(spec.validate(root).is_ok());
    }

    #[test]
    fn test_path_traversal_rejection() {
        let temp = TempDir::new().unwrap();
        let root = temp.path();

        let spec = SynapseSpec {
            schema_version: 1,
            run_id: "r1".into(),
            feature_id: "f1".into(),
            base_commit: "abc".into(),
            contract_revision: 1,
            contract_files: vec!["../../etc/passwd".into()],
            contract_tests: vec![],
            partitions: BTreeMap::new(),
            shared_file_policy: BTreeMap::new(),
            shared_files: vec![],
            verification_commands: vec![],
            limits: LimitsConfig::default(),
            approval_policy: ApprovalPolicyConfig::default(),
        };

        let err = spec.validate(root).unwrap_err();
        assert!(matches!(err, ManifestError::PathOutsideRepo(_)));
    }
}
