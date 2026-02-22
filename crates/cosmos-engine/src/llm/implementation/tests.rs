use super::*;
use std::process::Command as StdCommand;
use tempfile::tempdir;

#[test]
fn plain_language_gate_rejects_jargony_text() {
    assert!(!is_plain_language_text(
        "Updated src/app.rs by changing fn do_work() -> Result<()> and serde impl details"
    ));
}

#[test]
fn plain_language_gate_accepts_short_user_facing_text() {
    assert!(is_plain_language_text(
        "Users now see a clear error instead of a silent failure in this flow."
    ));
}

#[test]
fn diff_line_parser_ignores_headers() {
    let sample = "\
diff --git a/a.rs b/a.rs
--- a/a.rs
+++ b/a.rs
@@ -1 +1 @@
-old
+new
";
    assert_eq!(parse_diff_changed_lines(sample), 2);
}

#[test]
fn model_policy_uses_smart_tier() {
    assert_eq!(IMPLEMENTATION_MODEL.id(), "openai/gpt-oss-120b");
}

#[test]
fn model_policy_rejects_non_implementation_model() {
    assert!(ensure_implementation_model(Model::Smart).is_ok());
    assert!(ensure_implementation_model(Model::Speed).is_err());
}

#[test]
fn deterministic_scope_gate_rejects_out_of_scope_files() {
    let changed_files = vec![PathBuf::from("src/a.rs"), PathBuf::from("src/b.rs")];
    let allowed_files = HashSet::from([PathBuf::from("src/a.rs")]);
    assert!(!deterministic_scope_gate(&changed_files, &allowed_files));
}

#[test]
fn deterministic_scope_gate_allows_empty_changeset() {
    let changed_files: Vec<PathBuf> = Vec::new();
    let allowed_files = HashSet::from([PathBuf::from("src/a.rs")]);
    assert!(deterministic_scope_gate(&changed_files, &allowed_files));
}

#[test]
fn normalize_repo_change_path_rejects_empty_and_dot() {
    assert!(normalize_repo_change_path("").is_none());
    assert!(normalize_repo_change_path("   ").is_none());
    assert!(normalize_repo_change_path(".").is_none());
    assert!(normalize_repo_change_path("./").is_none());
}

#[test]
fn normalize_repo_change_path_strips_leading_dot_slash() {
    assert_eq!(
        normalize_repo_change_path("./src/main.rs"),
        Some(PathBuf::from("src/main.rs"))
    );
    assert_eq!(
        normalize_repo_change_path("src/lib.rs"),
        Some(PathBuf::from("src/lib.rs"))
    );
}

#[test]
fn revert_out_of_scope_changes_restores_repo_state() {
    let root = tempdir().unwrap();
    run_git(root.path(), &["init"]);
    run_git(root.path(), &["config", "user.email", "cosmos@example.com"]);
    run_git(root.path(), &["config", "user.name", "Cosmos"]);

    std::fs::create_dir_all(root.path().join("src")).unwrap();
    std::fs::write(root.path().join("src/allowed.rs"), "pub fn a() {}\n").unwrap();
    std::fs::write(root.path().join("src/extra.rs"), "pub fn b() {}\n").unwrap();
    run_git(root.path(), &["add", "."]);
    run_git(root.path(), &["commit", "-m", "init"]);

    std::fs::write(
        root.path().join("src/allowed.rs"),
        "pub fn a() { println!(\"x\"); }\n",
    )
    .unwrap();
    std::fs::write(
        root.path().join("src/extra.rs"),
        "pub fn b() { println!(\"y\"); }\n",
    )
    .unwrap();
    std::fs::write(root.path().join("scratch.txt"), "tmp\n").unwrap();

    let mut changes = collect_repo_changes(root.path()).expect("collect changes");
    changes.files.sort();
    assert!(changes.files.contains(&PathBuf::from("src/allowed.rs")));
    assert!(changes.files.contains(&PathBuf::from("src/extra.rs")));
    assert!(changes.files.contains(&PathBuf::from("scratch.txt")));

    let out_of_scope = vec![PathBuf::from("src/extra.rs"), PathBuf::from("scratch.txt")];
    revert_out_of_scope_changes(root.path(), &changes, &out_of_scope).expect("revert");

    let mut after = collect_repo_changes(root.path()).expect("collect after");
    after.files.sort();
    assert_eq!(after.files, vec![PathBuf::from("src/allowed.rs")]);
}

#[test]
fn syntax_gate_rejects_parse_broken_outputs() {
    let root = tempdir().unwrap();
    std::fs::write(root.path().join("broken.rs"), "fn broken( {").unwrap();
    let result = syntax_gate(root.path(), &[PathBuf::from("broken.rs")]);
    assert!(result.is_err());
}

#[test]
fn binary_write_gate_rejects_binary_extension() {
    let root = tempdir().unwrap();
    std::fs::write(root.path().join("logo.png"), "not-a-real-image").unwrap();
    let result = binary_write_gate(root.path(), &[PathBuf::from("logo.png")]);
    assert!(result.is_err());
}

#[test]
fn quick_checks_disabled_returns_unavailable() {
    let root = tempdir().unwrap();
    let (status, command, outcome) = run_quick_checks(
        root.path(),
        None,
        &mut Vec::new(),
        ImplementationQuickChecksMode::Disabled,
        100,
    )
    .unwrap();
    assert_eq!(status, ImplementationQuickCheckStatus::Unavailable);
    assert!(command.is_none());
    assert!(outcome.is_none());
}

#[test]
fn quick_check_policy_matrix_matches_profiles() {
    let interactive = ImplementationHarnessConfig::interactive_strict();
    let lab = ImplementationHarnessConfig::lab_strict();
    assert!(!interactive.require_quick_check_detectable);
    assert!(lab.require_quick_check_detectable);
    assert_eq!(
        interactive.adversarial_review_model,
        ImplementationReviewModel::Smart
    );
    assert_eq!(
        lab.adversarial_review_model,
        ImplementationReviewModel::Smart
    );
    assert!(interactive.require_independent_review_on_pass);
    assert!(!lab.require_independent_review_on_pass);
    assert_eq!(interactive.max_auto_review_fix_loops, 4);
    assert_eq!(lab.max_auto_review_fix_loops, 8);
    assert_eq!(interactive.max_auto_quick_check_fix_loops, 2);
    assert_eq!(lab.max_auto_quick_check_fix_loops, 6);
    assert_eq!(interactive.max_smart_escalations_per_attempt, 2);
    assert_eq!(lab.max_smart_escalations_per_attempt, 2);
    assert_eq!(interactive.reserve_independent_review_ms, 8_000);
    assert_eq!(lab.reserve_independent_review_ms, 8_000);
    assert!((interactive.reserve_independent_review_cost_usd - 0.0015).abs() < 1e-9);
    assert!((lab.reserve_independent_review_cost_usd - 0.0015).abs() < 1e-9);
    assert!(!interactive.enable_quick_check_baseline);
    assert!(!lab.enable_quick_check_baseline);
    assert!(quick_check_passes_policy(
        ImplementationQuickCheckStatus::Unavailable,
        &interactive
    ));
    assert!(!quick_check_passes_policy(
        ImplementationQuickCheckStatus::Unavailable,
        &lab
    ));
}

#[test]
fn adversarial_review_model_policy_allows_smart_only() {
    assert!(ensure_adversarial_review_model(Model::Smart).is_ok());
    assert!(ensure_adversarial_review_model(Model::Speed).is_err());
}

#[test]
fn generation_model_policy_allows_smart_only() {
    assert!(ensure_generation_model(Model::Smart).is_ok());
    assert!(ensure_generation_model(Model::Speed).is_err());
}

#[test]
fn response_format_schema_error_detector_matches_provider_message() {
    assert!(is_response_format_schema_error_text(
        "API error 400 Bad Request: Invalid schema for response_format 'review_response'"
    ));
    assert!(!is_response_format_schema_error_text(
        "API error 429 Too Many Requests: Rate limited"
    ));
}

#[test]
fn generation_escalation_reason_detects_placeholder_ellipsis() {
    let reason = generation_escalation_reason(
        "Edit 2: old_string contains placeholder ellipsis. Copy exact code; do not use `...`.",
    );
    assert_eq!(reason, Some("placeholder_ellipsis_anchor"));
}

#[test]
fn quick_check_skips_next_lint_on_next16_and_falls_back_to_build() {
    let root = tempdir().unwrap();
    std::fs::write(
        root.path().join("package.json"),
        r#"{
  "name": "x",
  "private": true,
  "scripts": { "lint": "next lint", "build": "next build" },
  "dependencies": { "next": "^16.1.1" }
}"#,
    )
    .unwrap();

    let command = detect_quick_check_command(root.path()).expect("expected check command");
    match command {
        QuickCheckCommand::Program { program, args } => {
            assert_eq!(program, "npm");
            assert_eq!(
                args,
                vec![
                    "run".to_string(),
                    "build".to_string(),
                    "--silent".to_string()
                ]
            );
        }
        _ => panic!("expected program quick check"),
    }
}

#[test]
fn quick_check_skips_eslint_lint_when_eslint_missing_and_prefers_build() {
    let root = tempdir().unwrap();
    std::fs::write(root.path().join("pnpm-lock.yaml"), "").unwrap();
    std::fs::write(
        root.path().join("package.json"),
        r#"{
  "name": "x",
  "private": true,
  "scripts": { "lint": "eslint .", "build": "next build" },
  "dependencies": { "next": "16.0.10" }
}"#,
    )
    .unwrap();

    let command = detect_quick_check_command(root.path()).expect("expected check command");
    match command {
        QuickCheckCommand::Program { program, args } => {
            assert_eq!(program, "pnpm");
            assert_eq!(args, vec!["build".to_string()]);
        }
        _ => panic!("expected program quick check"),
    }
}

#[test]
fn quick_check_prefers_test_lint_over_heavy_test_aggregator() {
    let root = tempdir().unwrap();
    std::fs::write(root.path().join("pnpm-lock.yaml"), "").unwrap();
    std::fs::write(
        root.path().join("package.json"),
        r#"{
  "name": "x",
  "private": true,
  "scripts": {
    "test": "pnpm run /^test:/",
    "test:lint": "eslint .",
    "test:coverage": "c8 vitest"
  },
  "devDependencies": { "eslint": "^9.0.0" }
}"#,
    )
    .unwrap();

    let command = detect_quick_check_command(root.path()).expect("expected check command");
    match command {
        QuickCheckCommand::Program { program, args } => {
            assert_eq!(program, "pnpm");
            assert_eq!(args, vec!["test:lint".to_string()]);
        }
        _ => panic!("expected program quick check"),
    }
}

#[test]
fn quick_check_detects_rust_without_lockfile_as_unlocked_check() {
    let root = tempdir().unwrap();
    std::fs::write(
        root.path().join("Cargo.toml"),
        "[package]\nname = \"x\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();

    let command = detect_quick_check_command(root.path()).expect("expected check command");
    match command {
        QuickCheckCommand::Program { program, args } => {
            assert_eq!(program, "cargo");
            assert_eq!(args, vec!["check".to_string()]);
        }
        _ => panic!("expected cargo quick check"),
    }
}

#[test]
fn quick_check_detects_rust_with_lockfile_as_locked_check() {
    let root = tempdir().unwrap();
    std::fs::write(
        root.path().join("Cargo.toml"),
        "[package]\nname = \"x\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    std::fs::write(root.path().join("Cargo.lock"), "").unwrap();

    let command = detect_quick_check_command(root.path()).expect("expected check command");
    match command {
        QuickCheckCommand::Program { program, args } => {
            assert_eq!(program, "cargo");
            assert_eq!(args, vec!["check".to_string(), "--locked".to_string()]);
        }
        _ => panic!("expected cargo quick check"),
    }
}

#[test]
fn quick_check_detects_python_compileall_from_pyproject() {
    let root = tempdir().unwrap();
    std::fs::write(
        root.path().join("pyproject.toml"),
        "[project]\nname = \"x\"\n",
    )
    .unwrap();

    let command = detect_quick_check_command(root.path()).expect("expected check command");
    match command {
        QuickCheckCommand::Program { program, args } => {
            assert_eq!(program, "python3");
            assert_eq!(
                args,
                vec![
                    "-m".to_string(),
                    "compileall".to_string(),
                    "-q".to_string(),
                    ".".to_string()
                ]
            );
        }
        _ => panic!("expected python quick check"),
    }
}

#[test]
fn quick_check_requires_real_node_modules_for_typecheck_script() {
    let root = tempdir().unwrap();
    std::fs::write(
        root.path().join("package.json"),
        r#"{
  "name": "x",
  "private": true,
  "scripts": { "typecheck": "tsc -p ." }
}"#,
    )
    .unwrap();

    let command = detect_quick_check_command(root.path()).expect("expected check command");
    assert!(quick_check_requires_real_node_modules(
        root.path(),
        &command
    ));
}

#[test]
fn gate_reason_records_capture_gate_and_code() {
    let mut reasons = Vec::new();
    let mut records = Vec::new();
    push_fail_reason(
        &mut reasons,
        &mut records,
        "quick_check",
        REASON_QUICK_CHECK_UNAVAILABLE,
        "check command unavailable",
    );
    let mut gates = Vec::new();
    push_gate(
        &mut gates,
        "quick_check",
        false,
        "No detectable quick-check command",
        Some(REASON_QUICK_CHECK_UNAVAILABLE),
    );

    assert_eq!(reasons.len(), 1);
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].gate, "quick_check");
    assert_eq!(records[0].code, REASON_QUICK_CHECK_UNAVAILABLE);
    assert!(!records[0].action.trim().is_empty());
    assert!(records[0]
        .action
        .contains("Install or enable the required quick-check tool"));
    assert_eq!(
        gates[0].reason_code.as_deref(),
        Some(REASON_QUICK_CHECK_UNAVAILABLE)
    );
}

#[test]
fn quick_check_repair_hint_extracts_rust_e0277() {
    let summary = "Quick check failed (cargo check): error[E0277]: the `?` operator can only be used in a function that returns `Result` or `Option`";
    let hint = quick_check_repair_hint_from_summary(summary).expect("expected hint");
    assert!(hint.contains("Rust E0277 hint"));
    assert!(hint.contains("remove `?`"));
}

#[test]
fn quick_check_failure_summary_extracts_next_ts_error() {
    let outcome = ImplementationCommandOutcome {
            command: "npm run build --silent".to_string(),
            duration_ms: 0,
            success: false,
            timed_out: false,
            exit_code: Some(1),
            stdout_tail: String::new(),
            stderr_tail: "Failed to compile.\n\n./lib/constants.ts:60:44\nType error: Cannot find name 'FontPreferenceId'.\n".to_string(),
        };

    let summary = summarize_quick_check_failure(&outcome).expect("expected summary");
    assert!(summary.contains("npm run build --silent"));
    assert!(summary.contains("lib/constants.ts:60:44"));
    assert!(summary.contains("Cannot find name 'FontPreferenceId'"));
}

#[test]
fn quick_check_failure_summary_extracts_tsc_format() {
    let outcome = ImplementationCommandOutcome {
        command: "pnpm type-check".to_string(),
        duration_ms: 0,
        success: false,
        timed_out: false,
        exit_code: Some(2),
        stdout_tail: "src/foo.ts(12,34): error TS2304: Cannot find name 'X'.\n".to_string(),
        stderr_tail: String::new(),
    };

    let summary = summarize_quick_check_failure(&outcome).expect("expected summary");
    assert!(summary.contains("pnpm type-check"));
    assert!(summary.contains("src/foo.ts:12:34"));
    assert!(summary.contains("Cannot find name 'X'"));
}

#[test]
fn quick_check_failure_summary_prefers_fail_over_passing_error_lines() {
    let outcome = ImplementationCommandOutcome {
        command: "pnpm test".to_string(),
        duration_ms: 0,
        success: false,
        timed_out: false,
        exit_code: Some(1),
        stdout_tail: "✔ handles error cases\nFAIL src/foo.test.ts\nTypeError: boom\n".to_string(),
        stderr_tail: String::new(),
    };

    let summary = summarize_quick_check_failure(&outcome).expect("expected summary");
    assert!(summary.contains("pnpm test"));
    assert!(summary.contains("FAIL"), "{}", summary);
}

#[test]
fn quick_check_failure_summary_prefers_size_limit_and_lint_details_over_elifecycle() {
    let outcome = ImplementationCommandOutcome {
        command: "pnpm test".to_string(),
        duration_ms: 0,
        success: false,
        timed_out: false,
        exit_code: Some(1),
        stdout_tail: "\
. test:size:   Package size limit has exceeded by 25 B\n\
. test:size: Failed\n\
ELIFECYCLE Command failed with exit code 1.\n\
. test:lint:   13:24  error  Prefer `node:crypto` over `crypto`  n/prefer-node-protocol\n"
            .to_string(),
        stderr_tail: String::new(),
    };

    let summary = summarize_quick_check_failure(&outcome).expect("expected summary");
    assert!(
        summary.contains("Package size limit has exceeded"),
        "{}",
        summary
    );
    assert!(summary.contains("Prefer `node:crypto`"), "{}", summary);
}

#[test]
fn quick_check_failure_summary_extracts_coverage_threshold_failure() {
    let outcome = ImplementationCommandOutcome {
        command: "pnpm test".to_string(),
        duration_ms: 0,
        success: false,
        timed_out: false,
        exit_code: Some(1),
        stdout_tail: "\
. test:coverage: ERROR: Coverage for lines (98.48%) does not meet global threshold (100%)\n\
ELIFECYCLE Command failed with exit code 1.\n"
            .to_string(),
        stderr_tail: String::new(),
    };

    let summary = summarize_quick_check_failure(&outcome).expect("expected summary");
    assert!(summary.contains("Coverage for lines"), "{}", summary);
    assert!(
        summary.contains("does not meet global threshold"),
        "{}",
        summary
    );
}

#[test]
fn eslint_fixable_failure_detector_matches_common_eslint_output() {
    let outcome = ImplementationCommandOutcome {
        command: "pnpm test:lint".to_string(),
        duration_ms: 0,
        success: false,
        timed_out: false,
        exit_code: Some(1),
        stdout_tail: "\
/tmp/repo\n\
> eslint .\n\
/tmp/repo/index.js\n\
  20:3  error  `const` declaration outside top-level scope  prefer-let/prefer-let\n\
\n\
✖ 1 problem (1 error, 0 warnings)\n\
  1 error and 0 warnings potentially fixable with the `--fix` option.\n\
ELIFECYCLE Command failed with exit code 1.\n"
            .to_string(),
        stderr_tail: String::new(),
    };
    assert!(is_eslint_fixable_failure(&outcome));
}

#[test]
fn quick_check_error_path_extraction_handles_multiple_formats_and_rejects_traversal() {
    let outcome = ImplementationCommandOutcome {
            command: "pnpm type-check".to_string(),
            duration_ms: 0,
            success: false,
            timed_out: false,
            exit_code: Some(2),
            stdout_tail: "src/foo.ts(12,34): error TS2304: Cannot find name 'X'.\n../oops.ts(1,1): error TS2304: bad\n"
                .to_string(),
            stderr_tail: "--> src/main.rs:7:9\nerror[E0425]: cannot find value\n".to_string(),
        };

    let paths = extract_quick_check_error_paths(&outcome, Path::new("."));
    assert!(paths.contains(&PathBuf::from("src/foo.ts")), "{:?}", paths);
    assert!(paths.contains(&PathBuf::from("src/main.rs")), "{:?}", paths);
    assert!(!paths.contains(&PathBuf::from("../oops.ts")), "{:?}", paths);
}

#[test]
fn quick_check_error_path_extraction_parses_node_stack_traces() {
    let outcome = ImplementationCommandOutcome {
            command: "pnpm test".to_string(),
            duration_ms: 0,
            success: false,
            timed_out: false,
            exit_code: Some(1),
            stdout_tail: "TypeError: boom\n    at foo (src/bar.ts:12:34)\n    at src/baz.js:1:2\n    at foo (/Users/me/project/abs.ts:9:9)\n".to_string(),
            stderr_tail: String::new(),
        };

    let paths = extract_quick_check_error_paths(&outcome, Path::new("."));
    assert!(paths.contains(&PathBuf::from("src/bar.ts")), "{:?}", paths);
    assert!(paths.contains(&PathBuf::from("src/baz.js")), "{:?}", paths);
    assert!(!paths.iter().any(|p| p.is_absolute()), "{:?}", paths);
}

#[test]
fn quick_check_error_path_extraction_parses_prefixed_eslint_output() {
    let root = tempdir().unwrap();
    let file = root.path().join("index.js");
    std::fs::write(&file, "const x = 1;\n").unwrap();

    let outcome = ImplementationCommandOutcome {
        command: "pnpm test".to_string(),
        duration_ms: 0,
        success: false,
        timed_out: false,
        exit_code: Some(1),
        stdout_tail: format!(
            ". test:lint: {}\n. test:lint:   26:5  error  no-undef\n",
            file.display()
        ),
        stderr_tail: String::new(),
    };

    let paths = extract_quick_check_error_paths(&outcome, root.path());
    assert!(paths.contains(&PathBuf::from("index.js")), "{:?}", paths);

    let locations = extract_quick_check_error_locations(&outcome, root.path());
    assert!(
        locations
            .iter()
            .any(|(path, ln, col)| path == &PathBuf::from("index.js") && *ln == 26 && *col == 5),
        "{:?}",
        locations
    );
}

#[test]
fn budget_exhausted_triggers_cost_gate() {
    let budget = ImplementationBudget {
        started_at: std::time::Instant::now(),
        max_total_ms: u64::MAX,
        max_total_cost_usd: 0.01,
    };
    let usage = Usage {
        cost: Some(0.02),
        ..Usage::default()
    };

    let reason = budget
        .exhausted(&Some(usage))
        .expect("expected budget to be exhausted");
    assert_eq!(reason.gate, "budget");
    assert_eq!(reason.code, REASON_BUDGET_EXCEEDED);
    assert!(reason.message.to_ascii_lowercase().contains("cost"));
}

#[test]
fn budget_exhausted_allows_small_cost_overrun_tolerance() {
    let budget = ImplementationBudget {
        started_at: std::time::Instant::now(),
        max_total_ms: u64::MAX,
        max_total_cost_usd: 0.01,
    };
    let within_tolerance = Usage {
        cost: Some(0.0102),
        ..Usage::default()
    };
    assert!(
        budget.exhausted(&Some(within_tolerance)).is_none(),
        "small accounting jitter should not hard-fail budget gate"
    );

    let beyond_tolerance = Usage {
        cost: Some(0.0103),
        ..Usage::default()
    };
    assert!(
        budget.exhausted(&Some(beyond_tolerance)).is_some(),
        "material overrun should still fail budget gate"
    );
}

#[test]
fn quick_check_failure_fingerprint_normalizes_numbers() {
    let a = "Quick check failed (cargo check --locked): src/error.rs:471:39 error[E0277]";
    let b = "Quick check failed (cargo check --locked): src/error.rs:473:21 error[E0277]";
    assert_eq!(
        quick_check_failure_fingerprint(a),
        quick_check_failure_fingerprint(b)
    );
}

#[test]
fn budget_guard_cost_buffer_scales_for_small_attempt_budget() {
    let budget = ImplementationBudget {
        started_at: std::time::Instant::now(),
        max_total_ms: u64::MAX,
        max_total_cost_usd: 0.0061,
    };

    // Small attempt budgets should scale below the old fixed $0.004 floor.
    let expected_buffer = (0.0061 * MIN_REMAINING_BUDGET_USD_FOR_LLM_CALL_RATIO).clamp(
        MIN_REMAINING_BUDGET_USD_FOR_LLM_CALL_MIN,
        MIN_REMAINING_BUDGET_USD_FOR_LLM_CALL_MAX,
    );
    assert!(
        (budget.min_remaining_cost_buffer_usd() - expected_buffer).abs() < f64::EPSILON,
        "buffer={}",
        budget.min_remaining_cost_buffer_usd()
    );

    let usage_with_headroom = Some(Usage {
        cost: Some(0.0022), // remaining 0.0039
        ..Usage::default()
    });
    assert!(
        budget.guard_before_llm_call(&usage_with_headroom).is_none(),
        "guard should allow a call with meaningful remaining budget"
    );

    let usage_below_buffer = Some(Usage {
        cost: Some(0.00602), // remaining 0.00008
        ..Usage::default()
    });
    assert!(
        budget.guard_before_llm_call(&usage_below_buffer).is_some(),
        "guard should stop once remaining budget falls below minimum buffer"
    );
}

#[test]
fn budget_guard_time_buffer_scales_for_small_attempt_budget() {
    let budget = ImplementationBudget {
        started_at: std::time::Instant::now(),
        max_total_ms: 5_500,
        max_total_cost_usd: 1.0,
    };

    // Small attempt budgets should not use a fixed 6000ms minimum.
    assert!(
        budget.min_remaining_ms_buffer().clamp(
            MIN_REMAINING_BUDGET_MS_FOR_LLM_CALL_MIN,
            MIN_REMAINING_BUDGET_MS_FOR_LLM_CALL_MAX
        ) < MIN_REMAINING_BUDGET_MS_FOR_LLM_CALL_MAX
    );

    // With no elapsed time there should be enough remaining time to allow a call.
    assert!(budget.guard_before_llm_call(&None).is_none());
}

#[test]
fn attempt_budget_weights_sum_to_one_for_common_profiles() {
    for attempts in [1usize, 2, 3, 4, 6] {
        let weights = attempt_budget_weights(attempts);
        assert_eq!(weights.len(), attempts);
        let sum = weights.iter().sum::<f64>();
        assert!(
            (sum - 1.0).abs() < 1e-9,
            "attempts={}, weights={:?}, sum={}",
            attempts,
            weights,
            sum
        );
    }
}

#[test]
fn attempt_budget_partitioning_preserves_attempt2_budget_after_attempt1_spends_its_share() {
    let global_budget = ImplementationBudget {
        started_at: std::time::Instant::now(),
        max_total_ms: u64::MAX,
        max_total_cost_usd: 0.02,
    };
    let weights = attempt_budget_weights(4);

    // Simulate attempt 1 spending most of the total cost budget.
    let usage_so_far = Some(Usage {
        cost: Some(0.014),
        ..Usage::default()
    });
    let (_ms2, cost2) = compute_attempt_budget_caps(&global_budget, &usage_so_far, 2, &weights);
    assert!(
        (cost2 - 0.0033333333333333335).abs() < 1e-12,
        "expected attempt2 cost cap ~0.00333, got {}",
        cost2
    );
}

#[test]
fn compute_attempt_budget_caps_enforces_meaningful_floor_for_late_attempts() {
    let global_budget = ImplementationBudget {
        started_at: std::time::Instant::now(),
        max_total_ms: 10_000,
        max_total_cost_usd: 0.02,
    };
    let weights = attempt_budget_weights(4);
    let usage_so_far = Some(Usage {
        cost: Some(0.0179), // remaining = 0.0021
        ..Usage::default()
    });

    let (attempt_ms, attempt_cost) =
        compute_attempt_budget_caps(&global_budget, &usage_so_far, 3, &weights);

    // Late-attempt budget should still preserve a meaningful slice.
    assert!(
        (attempt_cost - 0.0021).abs() < 1e-9,
        "attempt_cost={}",
        attempt_cost
    );
    // Time floor should keep late attempts meaningful (not near-zero).
    assert!(attempt_ms >= MIN_REMAINING_BUDGET_MS_FOR_LLM_CALL_MIN);
}

fn run_git(repo_root: &Path, args: &[&str]) {
    let status = StdCommand::new("git")
        .current_dir(repo_root)
        .args(args)
        .status()
        .expect("run git");
    assert!(
        status.success(),
        "git {:?} failed with status {:?}",
        args,
        status
    );
}
