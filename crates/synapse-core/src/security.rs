use std::fs;
use std::path::{Component, Path, PathBuf};
use std::sync::LazyLock;

use regex::Regex;
use thiserror::Error;

#[derive(Error, Debug, Clone, PartialEq, Eq)]
pub enum SecurityError {
    #[error("Path traversal detected: '{path}' escapes repository root '{root}'")]
    PathTraversalDetected { path: String, root: String },

    #[error(
        "Symlink '{path}' points outside repository root: resolves to '{target}' (root: '{root}')"
    )]
    SymlinkEscapesRoot {
        path: String,
        target: String,
        root: String,
    },

    #[error("Invalid path: '{0}'")]
    InvalidPath(String),
}

// Compile regex patterns once using LazyLock
static ANTHROPIC_KEY_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"sk-ant-[a-zA-Z0-9_\-]{15,}").unwrap());

static OPENAI_KEY_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\bsk-[a-zA-Z0-9_\-]{20,}").unwrap());

static GITHUB_TOKEN_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\b(ghp_[a-zA-Z0-9]{36}|github_pat_[a-zA-Z0-9_]{22,})\b").unwrap()
});

static BEARER_TOKEN_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\bBearer\s+[a-zA-Z0-9_\-\.]{15,}").unwrap());

static PRIVATE_KEY_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"-----BEGIN [A-Z0-9 ]*PRIVATE KEY-----[\s\S]*?-----END [A-Z0-9 ]*PRIVATE KEY-----")
        .unwrap()
});

static GENERIC_SECRET_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"(?i)\b(password|secret|api_key|token|auth_token)\s*[:=]\s*['"]?([^\s'"]{8,})['"]?"#,
    )
    .unwrap()
});

/// Redacts sensitive API keys, tokens, credentials, and private keys from text.
pub fn scrub_secrets(input: &str) -> String {
    let mut scrubbed = input.to_string();

    scrubbed = ANTHROPIC_KEY_RE
        .replace_all(&scrubbed, "sk-ant-***REDACTED***")
        .to_string();
    scrubbed = OPENAI_KEY_RE
        .replace_all(&scrubbed, "sk-***REDACTED***")
        .to_string();
    scrubbed = GITHUB_TOKEN_RE
        .replace_all(&scrubbed, "***REDACTED_GH_TOKEN***")
        .to_string();
    scrubbed = BEARER_TOKEN_RE
        .replace_all(&scrubbed, "Bearer ***REDACTED***")
        .to_string();
    scrubbed = PRIVATE_KEY_RE
        .replace_all(&scrubbed, "[***REDACTED PRIVATE KEY***]")
        .to_string();

    // Redact generic secret assignments while keeping key name if not already redacted
    scrubbed = GENERIC_SECRET_RE
        .replace_all(&scrubbed, |caps: &regex::Captures| {
            let key = &caps[1];
            let val = &caps[2];
            if val.contains("***REDACTED") {
                caps[0].to_string()
            } else {
                format!("{key}=***REDACTED***")
            }
        })
        .to_string();

    scrubbed
}

/// Recursively scrubs sensitive values in serde_json::Value
pub fn scrub_json(val: &mut serde_json::Value) {
    match val {
        serde_json::Value::String(s) => {
            *s = scrub_secrets(s);
        }
        serde_json::Value::Array(arr) => {
            for v in arr {
                scrub_json(v);
            }
        }
        serde_json::Value::Object(map) => {
            for (k, v) in map.iter_mut() {
                let lower_k = k.to_lowercase();
                let is_sensitive = lower_k.contains("key")
                    || lower_k.contains("secret")
                    || lower_k.contains("token")
                    || lower_k.contains("password");
                if is_sensitive && let serde_json::Value::String(_) = v {
                    *v = serde_json::Value::String("***REDACTED***".into());
                    continue;
                }
                scrub_json(v);
            }
        }
        _ => {}
    }
}

/// Truncates string output to `max_bytes`.
/// If `keep_tail` is true, keeps the end of the text (useful for error/diagnostic logs).
pub fn truncate_output(text: &str, max_bytes: usize, keep_tail: bool) -> String {
    if text.len() <= max_bytes {
        return text.to_string();
    }

    if max_bytes == 0 {
        return String::new();
    }

    let notice = "\n[... truncated by Synapse context firewall ...]\n";
    if max_bytes <= notice.len() {
        return text[..max_bytes.min(text.len())].to_string();
    }

    let allowed_content_bytes = max_bytes - notice.len();

    if keep_tail {
        let skip_bytes = text.len() - allowed_content_bytes;
        // Find safe UTF-8 character boundary
        let mut start_idx = skip_bytes;
        while start_idx < text.len() && !text.is_char_boundary(start_idx) {
            start_idx += 1;
        }
        format!("{}{}", notice.trim_start(), &text[start_idx..])
    } else {
        let mut end_idx = allowed_content_bytes;
        while end_idx > 0 && !text.is_char_boundary(end_idx) {
            end_idx -= 1;
        }
        format!("{}{}", &text[..end_idx], notice.trim_end())
    }
}

/// Validates that a path resides strictly within the given root directory.
/// Prevents path traversal (`..`), absolute path escapes, and unsafe symlink targets.
pub fn validate_path_in_root(
    path: impl AsRef<Path>,
    root: impl AsRef<Path>,
) -> Result<PathBuf, SecurityError> {
    let path = path.as_ref();
    let root = root.as_ref();

    let path_str = path.to_string_lossy();
    // Normalize backslashes to forward slashes for cross-platform component checking
    let normalized_path_str = path_str.replace('\\', "/");
    let check_path = Path::new(&normalized_path_str);

    let has_win_drive = normalized_path_str.len() >= 2
        && normalized_path_str.as_bytes()[1] == b':'
        && normalized_path_str.as_bytes()[0].is_ascii_alphabetic();

    // Check for explicit .. traversal components
    for comp in check_path.components() {
        if comp == Component::ParentDir {
            let combined = root.join(check_path);
            let normalized = normalize_path(&combined);
            let norm_root = normalize_path(root);
            if !normalized.starts_with(&norm_root) {
                return Err(SecurityError::PathTraversalDetected {
                    path: path.to_string_lossy().to_string(),
                    root: root.to_string_lossy().to_string(),
                });
            }
        }
    }

    let is_absolute =
        check_path.is_absolute() || normalized_path_str.starts_with('/') || has_win_drive;

    let combined = if is_absolute {
        if has_win_drive && !cfg!(windows) {
            return Err(SecurityError::PathTraversalDetected {
                path: path.to_string_lossy().to_string(),
                root: root.to_string_lossy().to_string(),
            });
        }
        check_path.to_path_buf()
    } else {
        root.join(check_path)
    };

    let normalized = normalize_path(&combined);
    let norm_root = normalize_path(root);

    if !normalized.starts_with(&norm_root) {
        return Err(SecurityError::PathTraversalDetected {
            path: path.to_string_lossy().to_string(),
            root: root.to_string_lossy().to_string(),
        });
    }

    // Check if path exists and has symlinks pointing outside
    if combined.exists()
        && let Ok(canonical) = fs::canonicalize(&combined)
        && let Ok(canon_root) = fs::canonicalize(root)
    {
        // On Windows, canonicalize prepends \\?\; compare normalized representations
        let canon_str = canonical
            .to_string_lossy()
            .replace(r"\\?\", "")
            .replace('\\', "/");
        let root_str = canon_root
            .to_string_lossy()
            .replace(r"\\?\", "")
            .replace('\\', "/");

        let norm_root_prefix = if root_str.ends_with('/') {
            root_str
        } else {
            format!("{root_str}/")
        };

        if canon_str != norm_root_prefix.trim_end_matches('/')
            && !canon_str.starts_with(&norm_root_prefix)
        {
            return Err(SecurityError::SymlinkEscapesRoot {
                path: path.to_string_lossy().to_string(),
                target: canonical.to_string_lossy().to_string(),
                root: root.to_string_lossy().to_string(),
            });
        }
    }

    Ok(normalized)
}

/// Lexically normalizes a path without touching the filesystem.
pub fn normalize_path(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    let mut has_root = false;

    for comp in path.components() {
        match comp {
            Component::Prefix(p) => normalized.push(p.as_os_str()),
            Component::RootDir => {
                has_root = true;
                normalized.push(Component::RootDir.as_os_str());
            }
            Component::CurDir => {}
            Component::ParentDir => {
                if let Some(last) = normalized.components().next_back() {
                    match last {
                        Component::Normal(_) => {
                            normalized.pop();
                        }
                        Component::ParentDir => {
                            normalized.push("..");
                        }
                        Component::RootDir | Component::Prefix(_) => {
                            // Cannot pop above root/prefix
                        }
                        Component::CurDir => {}
                    }
                } else if !has_root {
                    normalized.push("..");
                }
            }
            Component::Normal(c) => normalized.push(c),
        }
    }
    normalized
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_scrub_secrets() {
        let input = "Calling Claude with sk-ant-api03-abcdef1234567890 and OpenAI sk-proj-1234567890abcdef1234567890";
        let scrubbed = scrub_secrets(input);
        assert!(!scrubbed.contains("sk-ant-api03-abcdef1234567890"));
        assert!(!scrubbed.contains("sk-proj-1234567890abcdef1234567890"));
        assert!(scrubbed.contains("sk-ant-***REDACTED***"));
        assert!(scrubbed.contains("sk-***REDACTED***"));

        let gh_input = "Token: ghp_111122223333444455556666777788889999 and Bearer eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9";
        let gh_scrubbed = scrub_secrets(gh_input);
        assert!(gh_scrubbed.contains("***REDACTED_GH_TOKEN***"));
        assert!(gh_scrubbed.contains("Bearer ***REDACTED***"));

        let pk_input =
            "-----BEGIN RSA PRIVATE KEY-----\nMIIEowIBAAKCAQEA0\n-----END RSA PRIVATE KEY-----";
        let pk_scrubbed = scrub_secrets(pk_input);
        assert_eq!(pk_scrubbed, "[***REDACTED PRIVATE KEY***]");
    }

    #[test]
    fn test_scrub_json() {
        let mut json = serde_json::json!({
            "api_key": "sk-ant-1234567890abcdefgh",
            "normal_field": "hello world",
            "nested": {
                "secret": "super_secret_value",
                "details": "using sk-1234567890123456789012"
            }
        });

        scrub_json(&mut json);
        assert_eq!(json["api_key"], "***REDACTED***");
        assert_eq!(json["nested"]["secret"], "***REDACTED***");
        assert!(
            json["nested"]["details"]
                .as_str()
                .unwrap()
                .contains("sk-***REDACTED***")
        );
    }

    #[test]
    fn test_truncate_output() {
        let text = "Line 1: start of output\n".to_string()
            + "Line 2: some middle details\n"
            + "Line 3: more logs here\n"
            + "Line 4: almost at the end\n"
            + "Line 5: final diagnostic output line\n";
        assert!(text.len() > 100);

        let truncated_tail = truncate_output(&text, 100, true);
        assert!(truncated_tail.contains("[... truncated by Synapse context firewall ...]"));
        assert!(truncated_tail.ends_with("Line 5: final diagnostic output line\n"));

        let truncated_head = truncate_output(&text, 100, false);
        assert!(truncated_head.contains("[... truncated by Synapse context firewall ...]"));
        assert!(truncated_head.starts_with("Line 1: start of output\n"));
    }

    #[test]
    fn test_validate_path_in_root() {
        let dir = tempdir().unwrap();
        let root = dir.path();

        // Valid relative child
        let valid = validate_path_in_root("src/index.ts", root);
        assert!(valid.is_ok());

        // Path traversal escaping root
        let escape = validate_path_in_root("../../etc/passwd", root);
        assert!(matches!(
            escape,
            Err(SecurityError::PathTraversalDetected { .. })
        ));

        // Windows style traversal
        let win_escape = validate_path_in_root(r"..\..\Windows\System32", root);
        assert!(matches!(
            win_escape,
            Err(SecurityError::PathTraversalDetected { .. })
        ));

        // Absolute path escape
        let abs_escape = validate_path_in_root("/etc/passwd", root);
        assert!(matches!(
            abs_escape,
            Err(SecurityError::PathTraversalDetected { .. })
        ));

        // Windows drive escape
        let drive_escape = validate_path_in_root(r"C:\Windows\System32", root);
        assert!(matches!(
            drive_escape,
            Err(SecurityError::PathTraversalDetected { .. })
        ));
    }
}
