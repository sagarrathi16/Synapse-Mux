# Security Policy

## Supported Versions

Synapse-Mux is currently in active pre-v1.0 development. Security patches are applied to the latest `main` branch.

| Version | Supported          |
| ------- | ------------------ |
| 0.1.x   | :white_check_mark: |

## Reporting a Vulnerability

We take the security of `synapse-mux` seriously. If you discover a security vulnerability or security-related defect, please **do not open a public issue**.

Instead, please report it via GitHub Private Vulnerability Reporting or email the maintainers directly at `sagarrathi808@gmail.com`.

Include:
- A clear description of the vulnerability.
- Steps to reproduce or proof-of-concept repository/script.
- Potential impact and affected platforms (Windows, Linux, macOS).

You should receive an acknowledgment within 48 hours.

## Trust Boundaries & Threat Model

Synapse-Mux supervises third-party coding agent CLIs (Claude Code, OpenAI Codex CLI) that execute commands and modify files:
1. **Workspace Boundary:** Worker processes run in isolated Git worktrees (`.synapse/worktrees/<run_id>/`). Worktree isolation protects against accidental working-tree mutations, but is **not** an OS security sandbox.
2. **Path Sanitization:** All input paths in `synapse-spec.json` and worker modifications are canonicalized. Path traversal (`../`), escaping symlinks, and out-of-boundary access are strictly rejected.
3. **Secret Redaction:**
   - Environment variables passed to child subprocesses are explicitly filtered.
   - Known secret tokens (`ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, Bearer tokens, private keys) are redacted from diagnostic logs, recorded event streams, and evidence bundles.
   - Diagnostic exports automatically scrub credential patterns before serialization.
4. **Non-Interactive Execution:** Untrusted user or agent prompts are **never** passed to shell interpreters (`sh -c` or `cmd.exe /c`). All executions use explicit `argv` vectors.
5. **No Telemetry by Default:** Synapse-Mux operates completely local-first. No source code, diffs, or prompts are transmitted to external servers other than configured agent providers.

