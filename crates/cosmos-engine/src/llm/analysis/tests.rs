use super::*;
use chrono::Utc;
use cosmos_adapters::cache::{Cache, SuggestionCoverageCache};
use cosmos_core::context::WorkContext;
use cosmos_core::index::{
    CodebaseIndex, FileIndex, FileSummary, Language, Pattern, Symbol, SymbolKind, Visibility,
};
use cosmos_core::suggest::{
    Priority, SuggestionKind, SuggestionSource, SuggestionValidationMetadata, VerificationState,
};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

fn test_suggestion(summary: &str) -> Suggestion {
    Suggestion::new(
        SuggestionKind::BugFix,
        Priority::Medium,
        std::path::PathBuf::from("src/lib.rs"),
        summary.to_string(),
        SuggestionSource::LlmDeep,
    )
    .with_validation_metadata(SuggestionValidationMetadata {
        claim_impact_class: Some("correctness".to_string()),
        ..Default::default()
    })
    .with_verification_state(VerificationState::Verified)
}

#[test]
fn non_summary_model_guard_rejects_speed() {
    assert!(ensure_non_summary_model(Model::Speed, "Suggestions").is_err());
    assert!(ensure_non_summary_model(Model::Smart, "Suggestions").is_ok());
}

fn temp_root(label: &str) -> PathBuf {
    let mut root = std::env::temp_dir();
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    root.push(format!("cosmos_analysis_test_{}_{}", label, nanos));
    fs::create_dir_all(&root).unwrap();
    root
}

fn write_fixture_file(root: &Path, rel: &str, lines: usize) {
    let full = root.join(rel);
    if let Some(parent) = full.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    let mut content = String::new();
    for i in 1..=lines.max(8) {
        content.push_str(&format!("fn line_{}() {{}}\n", i));
    }
    fs::write(full, content).unwrap();
}

fn mk_file_index(
    rel: &str,
    loc: usize,
    complexity: f64,
    patterns: Vec<Pattern>,
    symbols: Vec<Symbol>,
    used_by: usize,
) -> (PathBuf, FileIndex) {
    let path = PathBuf::from(rel);
    let index = FileIndex {
        path: path.clone(),
        language: Language::Rust,
        loc,
        content_hash: format!("hash-{}", rel),
        symbols,
        dependencies: Vec::new(),
        patterns,
        complexity,
        last_modified: Utc::now(),
        summary: FileSummary {
            purpose: "test file".to_string(),
            exports: Vec::new(),
            used_by: (0..used_by)
                .map(|i| PathBuf::from(format!("src/dep_{}.rs", i)))
                .collect(),
            depends_on: Vec::new(),
        },
        layer: None,
        feature: None,
    };
    (path, index)
}

fn empty_context(root: &Path) -> WorkContext {
    WorkContext {
        branch: "test".to_string(),
        uncommitted_files: Vec::new(),
        staged_files: Vec::new(),
        untracked_files: Vec::new(),
        inferred_focus: None,
        modified_count: 0,
        repo_root: root.to_path_buf(),
    }
}

fn run_git(root: &Path, args: &[&str]) {
    let status = Command::new("git")
        .current_dir(root)
        .args(args)
        .status()
        .expect("git command should start");
    assert!(
        status.success(),
        "git command failed: git {}",
        args.join(" ")
    );
}

fn init_git_repo(root: &Path) {
    run_git(root, &["init"]);
    run_git(root, &["config", "user.name", "Cosmos Tests"]);
    run_git(root, &["config", "user.email", "cosmos-tests@example.com"]);
}

fn commit_all(root: &Path, message: &str) {
    run_git(root, &["add", "."]);
    run_git(root, &["commit", "-m", message, "--quiet"]);
}

fn security_symbol(path: &Path, name: &str) -> Symbol {
    Symbol {
        name: name.to_string(),
        kind: SymbolKind::Function,
        file: path.to_path_buf(),
        line: 1,
        end_line: 2,
        complexity: 1.0,
        visibility: Visibility::Public,
    }
}

#[test]
fn rank_top_churn_files_falls_back_to_risk_scoring_when_history_unavailable() {
    let root = temp_root("churn_fallback");
    write_fixture_file(&root, "src/a.rs", 80);
    write_fixture_file(&root, "src/b.rs", 80);
    write_fixture_file(&root, "src/c.rs", 80);

    let mut files = HashMap::new();
    let (a_path, a_index) = mk_file_index("src/a.rs", 120, 12.0, Vec::new(), Vec::new(), 0);
    let (b_path, b_index) = mk_file_index("src/b.rs", 120, 45.0, Vec::new(), Vec::new(), 0);
    let (c_path, c_index) = mk_file_index("src/c.rs", 120, 28.0, Vec::new(), Vec::new(), 0);
    files.insert(a_path, a_index);
    files.insert(b_path.clone(), b_index);
    files.insert(c_path.clone(), c_index);

    let index = CodebaseIndex {
        root: root.clone(),
        files,
        index_errors: Vec::new(),
        git_head: None,
    };
    let context = empty_context(&root);

    let ranked = rank_top_churn_files_for_subagents(&root, &index, &context, 12, 2);
    assert_eq!(ranked.len(), 2);
    assert_eq!(ranked[0], b_path);
    assert_eq!(ranked[1], c_path);

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn shard_subagent_focus_files_balances_and_backfills_empty_shards() {
    let files = vec![
        PathBuf::from("src/a.rs"),
        PathBuf::from("src/b.rs"),
        PathBuf::from("src/c.rs"),
    ];
    let shards = shard_subagent_focus_files(&files, 4);
    assert_eq!(shards.len(), 4);
    assert!(shards.iter().all(|shard| !shard.is_empty()));
    assert_eq!(shards[0][0], PathBuf::from("src/a.rs"));
    assert_eq!(shards[1][0], PathBuf::from("src/b.rs"));
    assert_eq!(shards[2][0], PathBuf::from("src/c.rs"));
    assert_eq!(shards[3][0], PathBuf::from("src/a.rs"));
}

#[test]
fn hybrid_candidate_pool_respects_40_30_20_10_mix_with_disjoint_inputs() {
    let root = temp_root("hybrid_mix");
    init_git_repo(&root);

    let churn_paths = (0..12)
        .map(|idx| format!("src/churn/file_{idx}.rs"))
        .collect::<Vec<_>>();
    let security_paths = (0..9)
        .map(|idx| format!("src/security/auth_token_{idx}.rs"))
        .collect::<Vec<_>>();
    let complexity_paths = (0..6)
        .map(|idx| format!("src/complex/hotspot_{idx}.rs"))
        .collect::<Vec<_>>();
    let dormant_paths = (0..3)
        .map(|idx| format!("src/dormant/legacy_{idx}.rs"))
        .collect::<Vec<_>>();
    let filler_paths = (0..6)
        .map(|idx| format!("src/filler/file_{idx}.rs"))
        .collect::<Vec<_>>();

    for rel in churn_paths
        .iter()
        .chain(security_paths.iter())
        .chain(complexity_paths.iter())
        .chain(dormant_paths.iter())
        .chain(filler_paths.iter())
    {
        write_fixture_file(&root, rel, 40);
    }
    commit_all(&root, "initial");

    for round in 0..4 {
        for rel in &churn_paths {
            let full = root.join(rel);
            let mut existing = fs::read_to_string(&full).unwrap();
            existing.push_str(&format!("// churn round {round}\n"));
            fs::write(full, existing).unwrap();
        }
        commit_all(&root, &format!("churn-{round}"));
    }

    let mut files = HashMap::new();
    for rel in &churn_paths {
        let (path, index) = mk_file_index(rel, 120, 4.0, Vec::new(), Vec::new(), 0);
        files.insert(path, index);
    }
    for rel in &security_paths {
        let path_buf = PathBuf::from(rel);
        let symbols = vec![security_symbol(&path_buf, "validate_auth_token")];
        let (path, index) = mk_file_index(rel, 120, 6.0, Vec::new(), symbols, 0);
        files.insert(path, index);
    }
    for rel in &complexity_paths {
        let (path, index) = mk_file_index(rel, 200, 140.0, Vec::new(), Vec::new(), 0);
        files.insert(path, index);
    }
    for rel in &dormant_paths {
        let (path, index) = mk_file_index(rel, 60, 3.0, Vec::new(), Vec::new(), 0);
        files.insert(path, index);
    }
    for rel in &filler_paths {
        let (path, index) = mk_file_index(rel, 80, 1.0, Vec::new(), Vec::new(), 0);
        files.insert(path, index);
    }

    let index = CodebaseIndex {
        root: root.clone(),
        files,
        index_errors: Vec::new(),
        git_head: None,
    };
    let context = empty_context(&root);

    let mut coverage = SuggestionCoverageCache::new();
    let previously_scanned = churn_paths
        .iter()
        .chain(security_paths.iter())
        .chain(complexity_paths.iter())
        .chain(filler_paths.iter())
        .map(PathBuf::from)
        .collect::<Vec<_>>();
    coverage.record_scan(previously_scanned);
    Cache::new(&root)
        .save_suggestion_coverage_cache(&coverage)
        .unwrap();

    let pool = build_hybrid_candidate_pool(&root, &index, &context);
    let expected_pool_size = HYBRID_CANDIDATE_POOL_SIZE.min(index.files.len());
    assert_eq!(pool.len(), expected_pool_size);

    let churn_count = pool
        .iter()
        .filter(|path| path.to_string_lossy().contains("src/churn/"))
        .count();
    let security_count = pool
        .iter()
        .filter(|path| path.to_string_lossy().contains("src/security/"))
        .count();
    let complexity_count = pool
        .iter()
        .filter(|path| path.to_string_lossy().contains("src/complex/"))
        .count();
    let dormant_count = pool
        .iter()
        .filter(|path| path.to_string_lossy().contains("src/dormant/"))
        .count();

    assert_eq!(churn_count, 12);
    assert_eq!(security_count, 9);
    assert_eq!(complexity_count, 6);
    assert_eq!(dormant_count, 3);

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn hybrid_candidate_pool_dormant_rotation_persists_across_runs() {
    let root = temp_root("dormant_rotation");
    init_git_repo(&root);

    let churn_paths = (0..12)
        .map(|idx| format!("src/churn/file_{idx}.rs"))
        .collect::<Vec<_>>();
    let security_paths = (0..9)
        .map(|idx| format!("src/security/auth_{idx}.rs"))
        .collect::<Vec<_>>();
    let complexity_paths = (0..6)
        .map(|idx| format!("src/complex/hot_{idx}.rs"))
        .collect::<Vec<_>>();
    let dormant_paths = (0..12)
        .map(|idx| format!("src/dormant/legacy_{idx}.rs"))
        .collect::<Vec<_>>();
    let filler_paths = (0..48)
        .map(|idx| format!("src/filler/extra_{idx}.rs"))
        .collect::<Vec<_>>();

    for rel in churn_paths
        .iter()
        .chain(security_paths.iter())
        .chain(complexity_paths.iter())
        .chain(dormant_paths.iter())
        .chain(filler_paths.iter())
    {
        write_fixture_file(&root, rel, 40);
    }
    commit_all(&root, "initial");

    for round in 0..3 {
        for rel in &churn_paths {
            let full = root.join(rel);
            let mut existing = fs::read_to_string(&full).unwrap();
            existing.push_str(&format!("// churn {round}\n"));
            fs::write(full, existing).unwrap();
        }
        commit_all(&root, &format!("churn-{round}"));
    }

    let mut files = HashMap::new();
    for rel in &churn_paths {
        let (path, index) = mk_file_index(rel, 120, 4.0, Vec::new(), Vec::new(), 0);
        files.insert(path, index);
    }
    for rel in &security_paths {
        let path_buf = PathBuf::from(rel);
        let symbols = vec![security_symbol(&path_buf, "check_authz")];
        let (path, index) = mk_file_index(rel, 120, 6.0, Vec::new(), symbols, 0);
        files.insert(path, index);
    }
    for rel in &complexity_paths {
        let (path, index) = mk_file_index(rel, 220, 130.0, Vec::new(), Vec::new(), 0);
        files.insert(path, index);
    }
    for rel in &dormant_paths {
        let (path, index) = mk_file_index(rel, 60, 3.0, Vec::new(), Vec::new(), 0);
        files.insert(path, index);
    }
    for rel in &filler_paths {
        let (path, index) = mk_file_index(rel, 40, 1.0, Vec::new(), Vec::new(), 0);
        files.insert(path, index);
    }

    let index = CodebaseIndex {
        root: root.clone(),
        files,
        index_errors: Vec::new(),
        git_head: None,
    };
    let context = empty_context(&root);
    let mut coverage = SuggestionCoverageCache::new();
    let non_dormant = churn_paths
        .iter()
        .chain(security_paths.iter())
        .chain(complexity_paths.iter())
        .map(PathBuf::from)
        .collect::<Vec<_>>();
    coverage.record_scan(non_dormant);
    Cache::new(&root)
        .save_suggestion_coverage_cache(&coverage)
        .unwrap();

    let first = build_hybrid_candidate_pool(&root, &index, &context);
    let first_dormant = first
        .iter()
        .filter(|path| path.to_string_lossy().contains("src/dormant/"))
        .cloned()
        .collect::<HashSet<_>>();
    let expected_dormant =
        (HYBRID_CANDIDATE_POOL_SIZE * HYBRID_DORMANT_PERCENT / 100).min(dormant_paths.len());
    assert!(first_dormant.len() >= expected_dormant);

    let second = build_hybrid_candidate_pool(&root, &index, &context);
    let second_dormant = second
        .iter()
        .filter(|path| path.to_string_lossy().contains("src/dormant/"))
        .cloned()
        .collect::<HashSet<_>>();
    assert!(second_dormant.len() >= expected_dormant);

    let persisted = Cache::new(&root)
        .load_suggestion_coverage_cache()
        .expect("coverage cache should persist");
    for path in first_dormant.iter().chain(second_dormant.iter()) {
        assert!(
            persisted.recently_scanned.contains_key(path),
            "coverage cache should include {}",
            path.display()
        );
    }

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn retryable_generation_error_matches_expected_provider_failures() {
    assert!(is_retryable_generation_error(&anyhow::anyhow!(
        "API returned empty response."
    )));
    assert!(is_retryable_generation_error(&anyhow::anyhow!(
        "Timed out after 18000ms."
    )));
    assert!(is_retryable_generation_error(&anyhow::anyhow!(
        "429 rate limited by upstream provider."
    )));
    assert!(is_retryable_generation_error(&anyhow::anyhow!(
        "Agent did not call report_back within iteration/time budget."
    )));
    assert!(is_retryable_generation_error(&anyhow::anyhow!(
        "Model returned text instead of calling report_back."
    )));
    assert!(is_retryable_generation_error(&anyhow::anyhow!(
        "Invalid agent explanation JSON: expected value at line 1 column 1"
    )));
    assert!(is_retryable_generation_error(&anyhow::anyhow!(
        "Invalid reviewer explanation JSON: expected value at line 1 column 1"
    )));
    assert!(is_retryable_generation_error(&anyhow::anyhow!(
        "Invalid report_back payload: report_back.explanation must be valid JSON object"
    )));
    assert!(!is_retryable_generation_error(&anyhow::anyhow!(
        "Failed to parse structured response."
    )));
}

#[test]
fn claim_grounding_prefers_observed_behavior_over_noisy_detail() {
    let suggestion = test_suggestion("Users may see failures.")
        .with_detail(
            "This detail contains narrative language that does not mirror code tokens.".to_string(),
        )
        .with_evidence(
            "10| if let Err(err) = send_metric() {\n11|     return Err(err);\n12| }".to_string(),
        )
        .with_validation_metadata(cosmos_core::suggest::SuggestionValidationMetadata {
            claim_observed_behavior: Some("if let Err(err) = send_metric()".to_string()),
            ..Default::default()
        });

    assert!(suggestion_claim_is_grounded_for_acceptance(&suggestion));
}

#[test]
fn prevalidation_ethos_filter_accepts_plain_language_actionable_description() {
    let suggestion = test_suggestion(
        "When someone syncs changes, the request can time out and the save never completes.",
    )
    .with_detail(
        "The network call awaits without a timeout guard, so dropped sockets keep the request open. Add a bounded timeout before awaiting and return a handled error path."
            .to_string(),
    )
    .with_evidence(
        " 91| let response = client.send(request).await?;\n 92| // missing timeout guard here"
            .to_string(),
    )
    .with_evidence_refs(vec![SuggestionEvidenceRef {
        snippet_id: 20,
        file: PathBuf::from("src/sync.rs"),
        line: 91,
    }]);

    assert!(
        deterministic_prevalidation_ethos_reason(&suggestion).is_none(),
        "clear user impact + concrete cause should pass ethos filter"
    );
}

#[test]
fn default_gate_config_is_balanced_high_volume() {
    let config = SuggestionQualityGateConfig::default();
    assert_eq!(config.min_final_count, 1);
    assert_eq!(config.max_final_count, 12);
    assert_eq!(config.max_suggest_cost_usd, 0.20);
    assert_eq!(config.max_suggest_ms, 180_000);
    assert_eq!(config.max_attempts, 4);
}

#[test]
fn gate_default_mapping_matches_expected_ranges() {
    let gate = SuggestionQualityGateConfig::default();
    assert_eq!(gate.min_final_count, 1);
    assert_eq!(gate.max_final_count, 12);
    assert_eq!(gate.max_attempts, 4);
}

#[test]
fn gate_snapshot_is_best_effort_when_ethos_actionable_is_below_final_count() {
    let config = SuggestionQualityGateConfig {
        min_final_count: 1,
        ..Default::default()
    };

    let mut suggestions = Vec::new();
    for i in 0..10usize {
        let mut suggestion = test_suggestion(&format!(
            "When someone saves draft {}, the action fails and the page keeps spinning.",
            i
        ))
        .with_line(i + 1)
        .with_detail(
            "The save branch retries on network errors without a timeout, so failed sockets keep requests open. Add a timeout and return a handled error state."
                .to_string(),
        )
        .with_evidence(
            " 10| let response = client.send(req).await?;\n 11| // no timeout around this await"
                .to_string(),
        )
        .with_evidence_refs(vec![SuggestionEvidenceRef {
            snippet_id: 100 + i,
            file: PathBuf::from(format!("src/save_{}.rs", i)),
            line: 10,
        }])
        .with_validation_state(SuggestionValidationState::Validated);
        suggestion.file = PathBuf::from(format!("src/save_{}.rs", i));
        if i == 9 {
            suggestion.summary = "src/save_9.rs line 10 fails and this is bad.".to_string();
        }
        suggestions.push(suggestion);
    }

    let gate = build_gate_snapshot(&config, &suggestions, 3_000, 0.01);
    assert!(gate.passed);
    assert!(gate.fail_reasons.is_empty());
    assert!(gate.ethos_actionable_count < gate.final_count);
}

#[test]
fn gate_snapshot_reports_fail_reasons_for_count_and_time() {
    let config = SuggestionQualityGateConfig {
        min_final_count: 3,
        ..Default::default()
    };
    let suggestions = vec![
        test_suggestion("one").with_validation_state(SuggestionValidationState::Validated),
        test_suggestion("two").with_validation_state(SuggestionValidationState::Validated),
    ];
    let gate = build_gate_snapshot(&config, &suggestions, config.max_suggest_ms + 1, 0.01);
    assert!(!gate.passed);
    assert!(!gate.fail_reasons.is_empty());
    assert!(gate
        .fail_reasons
        .iter()
        .any(|reason| reason.starts_with("final_count_below_min")));
    assert!(gate
        .fail_reasons
        .iter()
        .any(|reason| reason.starts_with("suggest_time_above_max")));
    assert_eq!(gate.final_count, 2);
}

#[test]
fn normalize_grounded_summary_avoids_dangling_when_users_titles() {
    let summary = normalize_grounded_summary(
            "When users",
            "When users submit malformed HTML, the raw message is passed through without escaping and can render unsafely in email clients.",
            42,
        );
    assert!(summary.len() >= SUMMARY_MIN_CHARS);
    assert_ne!(summary.to_ascii_lowercase(), "when users");
}

#[test]
fn normalize_grounded_summary_keeps_one_complete_sentence() {
    let summary = normalize_grounded_summary(
            "When the page hides, CLS errors are ignored, so layout-shift problems may go unnoticed. This matters because undetected CLS bugs can degrade user experience.",
            "CLS metric updates can fail silently during page hide events.",
            42,
        );
    assert!(summary.ends_with('.'));
    assert!(!summary.contains("This matters because"));
    assert!(!summary.contains("undetected CLS bugs can degrade user experience"));
}

#[test]
fn normalize_grounded_summary_rejects_fragment_sentence_endings() {
    let summary = normalize_grounded_summary(
        "When the review UI renders before the review state is.",
        "State is.",
        42,
    );
    assert!(summary.is_empty());
}

#[test]
fn normalize_grounded_summary_never_uses_generic_fallback_text() {
    let summary = normalize_grounded_summary("Fix issue", "Tiny", 42);
    let fallback =
        "when someone uses this flow, visible behavior can break. this matters because it can interrupt normal work.";
    assert_ne!(summary.to_ascii_lowercase(), fallback);
    assert!(summary.is_empty());
}

#[test]
fn normalize_grounded_summary_rewrites_low_information_summary_from_detail() {
    let summary = normalize_grounded_summary(
            "Fix issue",
            "Parsing failures currently return a default value silently, which hides bad input and makes debugging harder.",
            42,
        );
    let lower = summary.to_ascii_lowercase();
    assert_ne!(lower, "fix issue");
    assert!(lower.contains("parsing failures"));
}

#[test]
fn normalize_grounded_detail_does_not_inject_generic_user_impact_fallback() {
    let detail = normalize_grounded_detail(
        "Too short",
        "Cache writes are silently ignored in this path",
    );
    let lower = detail.to_ascii_lowercase();
    assert!(!lower.contains("this matters because"));
    assert!(!lower.contains("users can observe incorrect behavior"));
}

#[test]
fn readiness_annotation_penalizes_ungrounded_generic_claims() {
    let suggestion = test_suggestion("This path may fail.")
        .with_detail("This flow may fail.".to_string())
        .with_evidence(" 10| const retries = 2;\n 11| let total = retries + 1;\n".to_string())
        .with_line(11)
        .with_validation_state(SuggestionValidationState::Validated);

    let annotated = annotate_implementation_readiness(suggestion);
    let score = annotated
        .implementation_readiness_score
        .expect("score should be set");
    assert!(score < DEFAULT_MIN_IMPLEMENTATION_READINESS_SCORE);
    assert!(annotated
        .implementation_risk_flags
        .iter()
        .any(|flag| flag == "claim_not_grounded_in_snippet"));
    assert!(annotated
        .implementation_risk_flags
        .iter()
        .any(|flag| flag == "generic_or_low_information_description"));
}

#[test]
fn readiness_annotation_keeps_grounded_specific_claims_high() {
    let suggestion = test_suggestion(
        "create_dir_all(cache_dir) errors are ignored in this branch.",
    )
    .with_detail(
        "The create_dir_all(cache_dir) error branch swallows _err and continues.".to_string(),
    )
    .with_evidence(
        " 40| if let Err(_err) = std::fs::create_dir_all(cache_dir) {\n 41|   // ignored\n 42| }\n"
            .to_string(),
    )
    .with_line(40)
    .with_validation_state(SuggestionValidationState::Validated);

    let annotated = annotate_implementation_readiness(suggestion);
    let score = annotated
        .implementation_readiness_score
        .expect("score should be set");
    assert!(score >= DEFAULT_MIN_IMPLEMENTATION_READINESS_SCORE);
    assert!(!annotated
        .implementation_risk_flags
        .iter()
        .any(|flag| flag == "claim_not_grounded_in_snippet"));
}

#[test]
fn gate_snapshot_keeps_diversity_metrics_without_enforcing_file_gate() {
    let config = SuggestionQualityGateConfig {
        min_final_count: 4,
        ..Default::default()
    };
    let mut suggestions = Vec::new();
    for i in 0..config.min_final_count {
        let mut suggestion = test_suggestion(&format!("Distinct issue {}", i))
            .with_validation_state(SuggestionValidationState::Validated);
        suggestion.file = PathBuf::from("src/one_file.ts");
        suggestions.push(suggestion);
    }

    let gate = build_gate_snapshot(&config, &suggestions, 10_000, 0.01);
    assert!(gate.passed);
    assert!(gate.fail_reasons.is_empty());
    assert!(gate.dominant_file_ratio > 0.9);
    assert_eq!(gate.unique_file_count, 1);
}
