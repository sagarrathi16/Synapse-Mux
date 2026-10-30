# synapse-mux

> **Contract-First Cross-Model Agent Orchestrator for Claude Code & OpenAI Codex CLI**

## Quick Start

Initialize Synapse in any repository with zero questions:

```bash
npx synapse-mux setup
```

Run a dual-agent orchestration task:

```bash
npx synapse-mux run --prompt "Implement stripe checkout and postgres webhook handler"
```

## Features

- **Isolated Git Worktrees:** Executes Claude Code and Codex CLI in background worktrees without mutating your working tree.
- **Contract-First Safety:** Hard gates all shared interfaces and schema types before parallel execution begins.
- **7-Step Verification Pipeline:** Typecheck (`tsc --noEmit` / `cargo check`), contract tests, and cross-model code review.
- **Editor Agnostic:** Works with Cursor, VS Code, JetBrains, Zed, Neovim, or standard terminal.

