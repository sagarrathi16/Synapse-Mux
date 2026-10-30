# Synapse-Mux: 5-Arm Comparative Benchmark Report

> [!NOTE]
> Generated at: `2026-10-30T08:45:13.742113400+00:00` | Pre-Registration Version: `1.0.0` | Total Runs: `10`

> [!IMPORTANT]
> **PILOT VERDICT: GO (PASS)**
>
> - Primary Gate Passed: Median human integration time reduced by 88.9% (target >= 25.0%)
> - Quality Invariant Maintained: Arm C acceptance pass rate (100.0%) >= Baseline B (0.0%)
> - Secondary Gate Passed: Arm C acceptance pass rate (100.0%) >= Arm D (100.0%) with lower wall-clock time (35.0s vs 68.0s)

### 1. Primary Go/No-Go Decision Gate

Evaluation formula for human integration time reduction relative to uncoordinated Baseline B:

$$\text{Improvement} = \frac{\text{median}(H_B) - \text{median}(H_C)}{\text{median}(H_B)} \times 100\%$$

- **Baseline B Median Human Time:** `36.00 min`
- **Synapse-Mux Arm C Median Human Time:** `4.00 min`
- **Observed Improvement:** **`88.9%`** (Pre-registered threshold: `>= 25.0%`)

### 2. Multi-Arm Performance Summary

| Arm ID | Description | Runs | 1st Build Pass | Hidden Acceptance | Median Human (min) | Median Wall (s) | Cost ($) |
|:---|:---|:---:|:---:|:---:|:---:|:---:|:---:|
| `A` | Single Agent Baseline | 2 | 100.0% | 100.0% | 14.0 ± 0.0 | 42.0s | $1.16 |
| `B` | Uncoordinated Worktrees | 2 | 0.0% | 0.0% | 36.0 ± 0.0 | 55.0s | $1.90 |
| `C` | Synapse-Mux Heterogeneous | 2 | 100.0% | 100.0% | 4.0 ± 0.0 | 35.0s | $1.44 |
| `C-same` | Synapse-Mux Homogeneous | 2 | 100.0% | 100.0% | 5.2 ± 0.0 | 35.0s | $1.52 |
| `D` | Sequential Contract | 2 | 100.0% | 100.0% | 7.5 ± 0.0 | 68.0s | $1.26 |

### 3. Paired Task Differences (Arm C vs Baseline B)

| Task ID | Repeat | Baseline B Time | Arm C Time | Time Saved (min) | B Pass | C Pass |
|:---|:---:|:---:|:---:|:---:|:---:|:---:|
| `task-01-order-service` | #1 | 36.0 min | 4.0 min | **+32.0 min** | FAIL | PASS |
| `task-02-auth-session` | #1 | 36.0 min | 4.0 min | **+32.0 min** | FAIL | PASS |

