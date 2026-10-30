use std::path::Path;
use tokio::process::Command;

#[derive(Debug, Clone)]
pub struct DetectedStack {
    pub language: &'static str,
    pub package_manager: &'static str,
    pub typecheck_cmd: Vec<String>,
    pub test_cmd: Vec<String>,
    pub contract_file: &'static str,
    pub contract_test_file: &'static str,
    pub claude_globs: Vec<String>,
    pub codex_globs: Vec<String>,
    pub contract_stub: &'static str,
    pub contract_test_stub: &'static str,
}

impl DetectedStack {
    pub fn detect(repo_root: &Path) -> Self {
        // 1. Rust detection
        if repo_root.join("Cargo.toml").exists() {
            return Self {
                language: "Rust",
                package_manager: "cargo",
                typecheck_cmd: vec!["cargo".into(), "check".into()],
                test_cmd: vec!["cargo".into(), "test".into()],
                contract_file: "src/contracts.rs",
                contract_test_file: "tests/contracts_test.rs",
                claude_globs: vec!["src/api/**".into(), "src/handlers/**".into()],
                codex_globs: vec![
                    "src/db/**".into(),
                    "src/models/**".into(),
                    "src/workers/**".into(),
                ],
                contract_stub: "// Synapse-Mux Shared Rust Contract\npub trait AppService {\n    fn version(&self) -> &'static str;\n}\n",
                contract_test_stub: "#[test]\nfn test_contract_interface() {\n    assert!(true);\n}\n",
            };
        }

        // 2. TypeScript / Node.js detection
        if repo_root.join("package.json").exists() {
            let pm = if repo_root.join("pnpm-lock.yaml").exists() {
                "pnpm"
            } else if repo_root.join("bun.lockb").exists() || repo_root.join("bun.lock").exists() {
                "bun"
            } else if repo_root.join("yarn.lock").exists() {
                "yarn"
            } else {
                "npm"
            };

            let is_ts = repo_root.join("tsconfig.json").exists();
            if is_ts {
                return Self {
                    language: "TypeScript",
                    package_manager: pm,
                    typecheck_cmd: vec![pm.into(), "tsc".into(), "--noEmit".into()],
                    test_cmd: vec![pm.into(), "test".into()],
                    contract_file: "src/contracts/app.contract.ts",
                    contract_test_file: "tests/app.contract.test.ts",
                    claude_globs: vec!["src/api/**".into(), "src/services/**".into()],
                    codex_globs: vec!["src/workers/**".into(), "src/db/**".into()],
                    contract_stub: "// Synapse-Mux Shared Contract\nexport interface AppContext {\n  userId: string;\n  role: string;\n}\n",
                    contract_test_stub: "import { AppContext } from '../src/contracts/app.contract';\n\nexport function verify(c: AppContext): boolean {\n  return typeof c.userId === 'string';\n}\n",
                };
            } else {
                return Self {
                    language: "JavaScript",
                    package_manager: pm,
                    typecheck_cmd: vec!["node".into(), "--check".into(), "src/index.js".into()],
                    test_cmd: vec![pm.into(), "test".into()],
                    contract_file: "src/contracts/app.contract.js",
                    contract_test_file: "tests/app.contract.test.js",
                    claude_globs: vec!["src/api/**".into()],
                    codex_globs: vec!["src/workers/**".into()],
                    contract_stub: "/** Synapse-Mux Shared Contract */\nmodule.exports = {};\n",
                    contract_test_stub: "const contract = require('../src/contracts/app.contract');\nif (!contract) process.exit(1);\n",
                };
            }
        }

        // 3. Python detection
        if repo_root.join("pyproject.toml").exists() || repo_root.join("requirements.txt").exists()
        {
            return Self {
                language: "Python",
                package_manager: "pip",
                typecheck_cmd: vec!["python".into(), "-m".into(), "mypy".into(), ".".into()],
                test_cmd: vec!["pytest".into()],
                contract_file: "contracts/app_contract.py",
                contract_test_file: "tests/test_contract.py",
                claude_globs: vec!["api/**".into(), "routes/**".into()],
                codex_globs: vec!["db/**".into(), "workers/**".into()],
                contract_stub: "# Synapse-Mux Shared Python Contract\nclass AppContext:\n    user_id: str\n",
                contract_test_stub: "def test_contract():\n    assert True\n",
            };
        }

        // 4. Go detection
        if repo_root.join("go.mod").exists() {
            return Self {
                language: "Go",
                package_manager: "go",
                typecheck_cmd: vec!["go".into(), "vet".into(), "./...".into()],
                test_cmd: vec!["go".into(), "test".into(), "./...".into()],
                contract_file: "contracts/contract.go",
                contract_test_file: "contracts/contract_test.go",
                claude_globs: vec!["internal/api/**".into()],
                codex_globs: vec!["internal/worker/**".into(), "internal/db/**".into()],
                contract_stub: "package contracts\n\ntype AppContext struct {\n    UserID string\n}\n",
                contract_test_stub: "package contracts\n\nimport \"testing\"\n\nfunc TestContract(t *testing.T) {}\n",
            };
        }

        // 5. Default generic fallback
        Self {
            language: "Generic",
            package_manager: "git",
            typecheck_cmd: vec![],
            test_cmd: vec![],
            contract_file: "contracts/interface.json",
            contract_test_file: "tests/contract_test.sh",
            claude_globs: vec!["src/frontend/**".into(), "src/api/**".into()],
            codex_globs: vec!["src/backend/**".into(), "src/worker/**".into()],
            contract_stub: "{\n  \"$schema\": \"synapse-contract\",\n  \"version\": 1\n}\n",
            contract_test_stub: "#!/bin/sh\nexit 0\n",
        }
    }
}

#[derive(Debug, Clone)]
pub struct ToolchainStatus {
    pub git: Option<String>,
    pub claude: Option<String>,
    pub codex: Option<String>,
}

impl ToolchainStatus {
    pub async fn detect() -> Self {
        let git = probe_cli("git", &["--version"]).await;
        let claude = probe_cli("claude", &["--version"]).await;
        let codex = probe_cli("codex", &["--version"]).await;

        Self { git, claude, codex }
    }
}

async fn probe_cli(bin: &str, args: &[&str]) -> Option<String> {
    let output = Command::new(bin).args(args).output().await.ok()?;
    if output.status.success() {
        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if stdout.is_empty() {
            Some(format!("{} (detected)", bin))
        } else {
            Some(stdout)
        }
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_detect_rust() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("Cargo.toml"), "[package]\nname = \"foo\"\n").unwrap();
        let detected = DetectedStack::detect(dir.path());
        assert_eq!(detected.language, "Rust");
        assert_eq!(detected.package_manager, "cargo");
        assert_eq!(detected.contract_file, "src/contracts.rs");
    }

    #[test]
    fn test_detect_typescript_pnpm() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("package.json"), "{}").unwrap();
        std::fs::write(dir.path().join("pnpm-lock.yaml"), "").unwrap();
        std::fs::write(dir.path().join("tsconfig.json"), "{}").unwrap();
        let detected = DetectedStack::detect(dir.path());
        assert_eq!(detected.language, "TypeScript");
        assert_eq!(detected.package_manager, "pnpm");
        assert_eq!(detected.contract_file, "src/contracts/app.contract.ts");
    }

    #[test]
    fn test_detect_python() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("requirements.txt"), "pytest\n").unwrap();
        let detected = DetectedStack::detect(dir.path());
        assert_eq!(detected.language, "Python");
        assert_eq!(detected.package_manager, "pip");
        assert_eq!(detected.contract_file, "contracts/app_contract.py");
    }
}
