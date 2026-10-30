use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

use synapse_core::manifest::SharedFileRule;
use synapse_eval::mcp_broker::McpBroker;
use synapse_eval::{
    ArmKind, BenchmarkConfig, BenchmarkRunner, EvaluationReport, PreRegistration, TaskRunResult,
};
use tempfile::tempdir;

#[tokio::test]
async fn test_2_task_dry_run_benchmark_all_5_arms() {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let tasks_dir = manifest_dir.join("../../eval/tasks");
    let prereg_path = manifest_dir.join("../../eval/preregistration.json");

    let temp_out = tempdir().unwrap();
    let output_dir = temp_out.path().join("results");
    let blind_dir = temp_out.path().join("blind_review");

    let config = BenchmarkConfig {
        tasks_dir,
        output_dir: output_dir.clone(),
        blind_dir: blind_dir.clone(),
        repeats: 1, // 1 repeat per task for fast dry run (2 tasks * 5 arms = 10 runs)
        randomize_order: true,
        seed: 42,
        arms: ArmKind::all(),
        mock_mode: true,
        enable_mcp_broker: false,
    };

    let runner = BenchmarkRunner::new(config);
    let run_report = runner
        .run_benchmark()
        .await
        .expect("Benchmark execution failed");

    // 2 tasks * 5 arms * 1 repeat = 10 runs
    assert_eq!(run_report.total_runs, 10);
    assert_eq!(run_report.results.len(), 10);
    assert_eq!(run_report.blind_records.len(), 10);

    // Verify blinded diff files exist
    let blind_key_path = blind_dir.join("blind_key.json");
    assert!(blind_key_path.exists());
    let key_content = fs::read_to_string(&blind_key_path).unwrap();
    assert!(key_content.contains("blind_"));

    for record in &run_report.blind_records {
        let patch_path = blind_dir.join(&record.patch_filename);
        assert!(patch_path.exists(), "Patch file missing: {:?}", patch_path);
        let patch_text = fs::read_to_string(&patch_path).unwrap();
        assert!(patch_text.contains("# Blinded Diff Review Artifact"));
        // Ensure arm metadata is redacted
        assert!(!patch_text.contains("synapse: checkpoint claude"));
    }

    // Build EvaluationReport
    let prereg = PreRegistration::load_from_file(&prereg_path).unwrap();
    let report = EvaluationReport::build(&run_report.results, &prereg);
    report
        .save_to_dir(&output_dir)
        .expect("Failed to save report");

    // Verify summary.md and summary.json exist and are well-formed
    let summary_md = output_dir.join("summary.md");
    let summary_json = output_dir.join("summary.json");
    assert!(summary_md.exists());
    assert!(summary_json.exists());

    let md_text = fs::read_to_string(&summary_md).unwrap();
    assert!(md_text.contains("# Synapse-Mux: 5-Arm Comparative Benchmark Report"));
    assert!(md_text.contains("PILOT VERDICT:"));
    assert!(md_text.contains("Multi-Arm Performance Summary"));

    // Verify Arm summaries
    assert!(report.arms.contains_key("A"));
    assert!(report.arms.contains_key("B"));
    assert!(report.arms.contains_key("C"));
    assert!(report.arms.contains_key("C-same"));
    assert!(report.arms.contains_key("D"));

    // Arm C should have significantly lower human integration minutes than Arm B
    let median_b = report.arms["B"].human_integration_minutes.median;
    let median_c = report.arms["C"].human_integration_minutes.median;
    assert!(
        median_c < median_b,
        "Expected Arm C human time ({}) < Baseline B ({})",
        median_c,
        median_b
    );

    // Verify Go/No-Go Decision Gate is evaluated
    assert!(report.go_no_go.improvement_percentage >= 25.0);
    assert!(report.go_no_go.primary_gate_passed);
    assert!(report.go_no_go.passed);
}

#[test]
fn test_go_no_go_evaluator_logic() {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let prereg_path = manifest_dir.join("../../eval/preregistration.json");
    let prereg = PreRegistration::load_from_file(&prereg_path).unwrap();

    // Mock scenario where Arm C achieves 75% reduction in human integration time
    let results = vec![
        TaskRunResult {
            task_id: "task-01".into(),
            arm_id: "B".into(),
            repeat_index: 0,
            first_integration_build_pass: false,
            acceptance_criteria_pass: true,
            human_integration_minutes: 30.0,
            task_wall_clock_seconds: 60.0,
            post_integration_defects: 2,
            contract_violations_detected: 0,
            repair_attempts: 1,
            observed_tokens: 50_000,
            estimated_cost_usd: 0.75,
            timestamp: chrono::Utc::now(),
        },
        TaskRunResult {
            task_id: "task-01".into(),
            arm_id: "C".into(),
            repeat_index: 0,
            first_integration_build_pass: true,
            acceptance_criteria_pass: true,
            human_integration_minutes: 5.0,
            task_wall_clock_seconds: 35.0,
            post_integration_defects: 0,
            contract_violations_detected: 0,
            repair_attempts: 0,
            observed_tokens: 45_000,
            estimated_cost_usd: 0.65,
            timestamp: chrono::Utc::now(),
        },
        TaskRunResult {
            task_id: "task-01".into(),
            arm_id: "D".into(),
            repeat_index: 0,
            first_integration_build_pass: true,
            acceptance_criteria_pass: true,
            human_integration_minutes: 8.0,
            task_wall_clock_seconds: 65.0,
            post_integration_defects: 0,
            contract_violations_detected: 0,
            repair_attempts: 0,
            observed_tokens: 40_000,
            estimated_cost_usd: 0.60,
            timestamp: chrono::Utc::now(),
        },
    ];

    let report = EvaluationReport::build(&results, &prereg);
    assert!(report.go_no_go.passed);
    assert!(report.go_no_go.primary_gate_passed);
    // (30 - 5) / 30 = 83.33% > 25%
    assert!(report.go_no_go.improvement_percentage > 80.0);
}

#[test]
fn test_mcp_broker_feature_and_tools() {
    let dir = tempdir().unwrap();
    let mut policy = BTreeMap::new();
    policy.insert(
        "shared/config.json".into(),
        SharedFileRule {
            owner: "supervisor".into(),
            resolution: None,
        },
    );

    let broker = McpBroker::new("test-run".into(), dir.path().to_path_buf(), policy);

    // Test tool dispatch directly
    let claim_args = serde_json::json!({
        "path": "shared/config.json",
        "reason": "Updating ports",
        "task_id": "worker-1"
    });
    let claim_res = broker
        .dispatch_tool("claim_shared_file", &claim_args)
        .unwrap();
    assert_eq!(claim_res["allowed"], true);

    let compliance_args = serde_json::json!({
        "checkpoint_id": "chk-123"
    });
    let comp_res = broker
        .dispatch_tool("verify_contract_compliance", &compliance_args)
        .unwrap();
    assert_eq!(comp_res["status"], "Passed");
}
