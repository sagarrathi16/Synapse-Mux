# Contributing to Synapse-Mux

Thank you for contributing to `synapse-mux`!

## Developer Certificate of Origin (DCO)

To ensure licensing integrity for this open-source project without requiring a CLA, all contributions must include a DCO sign-off line in commit messages:

```
Signed-off-by: Full Name <email@example.com>
```

You can add this automatically when committing using `git commit -s`.

### DCO Text (Version 1.1)

By making a contribution to this project, I certify that:

(a) The contribution was created in whole or in part by me and I have the right to submit it under the open source license indicated in the file; or

(b) The contribution is based upon previous work that, to the best of my knowledge, is covered under an appropriate open source license and I have the right under that license to submit that work with modifications, whether created in whole or in part by me, under the same open source license (unless I am permitted to submit under a different license), as indicated in the file; or

(c) The contribution was provided directly to me by some other person who certified (a), (b) or (c) and I have not modified it.

(d) I understand and agree that this project and the contribution are public and that a record of the contribution (including all personal information I submit with it, including my sign-off) is maintained indefinitely and may be redistributed consistent with this project or the open source license(s) involved.

---

## Code Quality Gates

Before submitting any Pull Request, your changes must pass the following local quality gates:

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

## Architectural Guidelines
Refer to `AGENTS.md` and `Docs/Architecture & PRD.md` for architectural invariants:
- Zero C-bindings in MVP (standard Git CLI invoked with explicit `argv`).
- No TUI/Ratatui in Milestone 1–3 (Clap CLI only).
- Strict subprocess supervision with process group / Job Object cleanup.
- Contract-first validation: never execute parallel workers without an approved, hash-locked contract.

