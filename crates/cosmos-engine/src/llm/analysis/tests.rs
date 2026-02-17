use super::*;
use chrono::Utc;
use cosmos_adapters::config::SuggestionsProfile;
use cosmos_core::context::WorkContext;
use cosmos_core::index::{CodebaseIndex, FileIndex, FileSummary, Language, Pattern, Symbol};
use cosmos_core::suggest::{
    Priority, SuggestionKind, SuggestionSource, SuggestionValidationMetadata, VerificationState,
};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
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
fn gate_attempt_model_starts_fast_then_escalates_to_smart() {
    assert_eq!(gate_attempt_model(1), Model::Speed);
    assert_eq!(gate_attempt_model(2), Model::Smart);
    assert_eq!(gate_attempt_model(5), Model::Smart);
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
fn verified_bug_security_scope_requires_verified_bugfix_and_allowed_impact() {
    let good =
        test_suggestion("Verified bug").with_validation_state(SuggestionValidationState::Validated);
    assert!(suggestion_is_verified_bug_or_security(&good));

    let wrong_kind = Suggestion::new(
        SuggestionKind::Optimization,
        Priority::Medium,
        PathBuf::from("src/fast.rs"),
        "Not a bug".to_string(),
        SuggestionSource::LlmDeep,
    )
    .with_validation_metadata(SuggestionValidationMetadata {
        claim_impact_class: Some("correctness".to_string()),
        ..Default::default()
    })
    .with_verification_state(VerificationState::Verified)
    .with_validation_state(SuggestionValidationState::Validated);
    assert!(!suggestion_is_verified_bug_or_security(&wrong_kind));

    let wrong_impact = test_suggestion("Wrong impact")
        .with_validation_metadata(SuggestionValidationMetadata {
            claim_impact_class: Some("maintainability".to_string()),
            ..Default::default()
        })
        .with_validation_state(SuggestionValidationState::Validated);
    assert!(!suggestion_is_verified_bug_or_security(&wrong_impact));
}

#[test]
fn finalize_validated_suggestions_drops_pending_without_capping() {
    let mut input = (0..24)
        .map(|i| {
            test_suggestion(&format!("v{}", i))
                .with_validation_state(SuggestionValidationState::Validated)
        })
        .collect::<Vec<_>>();
    input
        .push(test_suggestion("pending").with_validation_state(SuggestionValidationState::Pending));

    let out = finalize_validated_suggestions(input);
    assert_eq!(out.len(), 24);
    assert!(out
        .iter()
        .all(|s| s.validation_state == SuggestionValidationState::Validated));
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
fn prevalidation_rejection_reason_catches_missing_and_duplicate_primary_evidence() {
    let mut chunk_seen_evidence_ids: HashSet<usize> = HashSet::new();
    let used_evidence_ids: HashSet<usize> = HashSet::new();

    let missing = test_suggestion("missing refs");
    let missing_reason =
        prevalidation_rejection_reason(&missing, &used_evidence_ids, &mut chunk_seen_evidence_ids)
            .expect("missing evidence should be rejected");
    assert!(missing_reason
        .reason
        .contains("Missing primary evidence ref"));
    assert!(missing_reason.evidence_id.is_none());

    let first = test_suggestion("first").with_evidence_refs(vec![SuggestionEvidenceRef {
        snippet_id: 3,
        file: PathBuf::from("src/a.rs"),
        line: 10,
    }]);
    assert!(prevalidation_rejection_reason(
        &first,
        &used_evidence_ids,
        &mut chunk_seen_evidence_ids
    )
    .is_none());

    let duplicate_in_chunk =
        test_suggestion("duplicate chunk").with_evidence_refs(vec![SuggestionEvidenceRef {
            snippet_id: 3,
            file: PathBuf::from("src/a.rs"),
            line: 10,
        }]);
    let duplicate_reason = prevalidation_rejection_reason(
        &duplicate_in_chunk,
        &used_evidence_ids,
        &mut chunk_seen_evidence_ids,
    )
    .expect("duplicate in batch should be rejected");
    assert!(duplicate_reason
        .reason
        .contains("Duplicate evidence_id in validation batch"));
    assert_eq!(duplicate_reason.evidence_id, Some(3));

    let mut chunk_seen_evidence_ids: HashSet<usize> = HashSet::new();
    let used_evidence_ids = HashSet::from([9usize]);
    let duplicate_used =
        test_suggestion("duplicate used").with_evidence_refs(vec![SuggestionEvidenceRef {
            snippet_id: 9,
            file: PathBuf::from("src/b.rs"),
            line: 22,
        }]);
    let duplicate_used_reason = prevalidation_rejection_reason(
        &duplicate_used,
        &used_evidence_ids,
        &mut chunk_seen_evidence_ids,
    )
    .expect("duplicate against used set should be rejected");
    assert!(duplicate_used_reason
        .reason
        .contains("Duplicate evidence_id already validated"));
    assert_eq!(duplicate_used_reason.evidence_id, Some(9));
}

#[test]
fn prevalidation_rejection_reason_rejects_unconfigured_client_id_claim_when_literal_exists() {
    let mut chunk_seen_evidence_ids: HashSet<usize> = HashSet::new();
    let used_evidence_ids: HashSet<usize> = HashSet::new();

    let suggestion = test_suggestion(
        "GitHub login can fail because the client id is not configured in this build.",
    )
    .with_evidence("31| const CLIENT_ID: &str = \"Ov23liBvoDPv3W7Dpjoz\";".to_string())
    .with_evidence_refs(vec![SuggestionEvidenceRef {
        snippet_id: 11,
        file: PathBuf::from("src/github.rs"),
        line: 31,
    }]);

    let reason = prevalidation_rejection_reason(
        &suggestion,
        &used_evidence_ids,
        &mut chunk_seen_evidence_ids,
    )
    .expect("configured client id contradiction should be rejected");

    assert!(reason.reason.contains("client ID appears configured"));
    assert_eq!(reason.evidence_id, Some(11));
    assert!(reason.is_contradiction);
}

#[test]
fn prevalidation_client_id_contradiction_allows_placeholder_value() {
    let suggestion = test_suggestion(
        "GitHub login can fail because the client id is not configured in this build.",
    )
    .with_evidence("31| const CLIENT_ID: &str = \"YOUR_CLIENT_ID_HERE\";".to_string());

    assert!(
        deterministic_prevalidation_contradiction_reason(&suggestion).is_none(),
        "placeholder client id should not be auto-contradicted"
    );
}

#[test]
fn prevalidation_rejection_reason_rejects_absolute_path_guard_false_positive() {
    let mut chunk_seen_evidence_ids: HashSet<usize> = HashSet::new();
    let used_evidence_ids: HashSet<usize> = HashSet::new();

    let suggestion = test_suggestion(
        "Absolute path handling fails and blocks users from opening projects with full paths.",
    )
    .with_evidence(
        "105| if candidate.is_absolute() {\n106|     return Err(format!(\"Absolute paths are not allowed\"));\n107| }"
            .to_string(),
    )
    .with_evidence_refs(vec![SuggestionEvidenceRef {
        snippet_id: 12,
        file: PathBuf::from("src/util.rs"),
        line: 105,
    }]);

    let reason = prevalidation_rejection_reason(
        &suggestion,
        &used_evidence_ids,
        &mut chunk_seen_evidence_ids,
    )
    .expect("absolute path guard contradiction should be rejected");

    assert!(reason.reason.contains("absolute-path security guard"));
    assert_eq!(reason.evidence_id, Some(12));
    assert!(reason.is_contradiction);
}

#[test]
fn prevalidation_rejection_reason_rejects_cache_not_created_false_positive() {
    let mut chunk_seen_evidence_ids: HashSet<usize> = HashSet::new();
    let used_evidence_ids: HashSet<usize> = HashSet::new();

    let suggestion = test_suggestion(
        "Cache operations crash because the cache directory is not automatically created.",
    )
    .with_evidence(
        "757| if exclusive {\n758|     self.ensure_dir()?;\n759| }\n724| fs::create_dir_all(&self.cache_dir)?;"
            .to_string(),
    )
    .with_evidence_refs(vec![SuggestionEvidenceRef {
        snippet_id: 13,
        file: PathBuf::from("src/cache.rs"),
        line: 757,
    }]);

    let reason = prevalidation_rejection_reason(
        &suggestion,
        &used_evidence_ids,
        &mut chunk_seen_evidence_ids,
    )
    .expect("cache directory contradiction should be rejected");

    assert!(reason
        .reason
        .contains("cache-directory creation/ensure logic"));
    assert_eq!(reason.evidence_id, Some(13));
    assert!(reason.is_contradiction);
}

#[test]
fn prevalidation_rejection_reason_rejects_non_actionable_safeguard_praise() {
    let mut chunk_seen_evidence_ids: HashSet<usize> = HashSet::new();
    let used_evidence_ids: HashSet<usize> = HashSet::new();

    let suggestion = test_suggestion(
        "Restoring files refuses malicious paths, preventing attackers from accessing arbitrary files.",
    )
    .with_evidence(
        "709| // Validate path to prevent traversal attacks\n710| let resolved = resolve_repo_path_allow_new(repo_path, file_path)\n711|     .map_err(|e| anyhow::anyhow!(\"Invalid path '{}': {}\", file_path.display(), e))?;\n105| if candidate.is_absolute() {\n106|     return Err(format!(\"Absolute paths are not allowed\"));\n107| }".to_string(),
    )
    .with_evidence_refs(vec![SuggestionEvidenceRef {
        snippet_id: 14,
        file: PathBuf::from("src/git_ops.rs"),
        line: 709,
    }]);

    let reason = prevalidation_rejection_reason(
        &suggestion,
        &used_evidence_ids,
        &mut chunk_seen_evidence_ids,
    )
    .expect("non-actionable safeguard praise should be rejected");

    assert!(reason
        .reason
        .contains("Non-actionable safeguard description"));
    assert_eq!(reason.evidence_id, Some(14));
    assert!(!reason.is_contradiction);
}

#[test]
fn prevalidation_safeguard_filter_allows_defect_risk_wording() {
    let suggestion = test_suggestion(
        "Path validation can still be bypassed, allowing traversal attacks in some flows.",
    )
    .with_evidence(
        "105| if candidate.is_absolute() {\n106|     return Err(format!(\"Absolute paths are not allowed\"));\n107| }"
            .to_string(),
    );

    assert!(
        deterministic_prevalidation_non_actionable_reason(&suggestion).is_none(),
        "defect-risk wording should not be treated as non-actionable praise"
    );
}

#[test]
fn prevalidation_non_security_praise_filter_requires_handling_signals() {
    let suggestion = test_suggestion(
        "Users get a clear setup error instead of a silent failure when credentials are missing.",
    )
    .with_evidence("84| fn setup_label() -> &'static str {\n85|     \"setup\"\n86| }".to_string());

    assert!(
        deterministic_prevalidation_non_actionable_reason(&suggestion).is_none(),
        "without explicit handling signals in snippet, non-security praise should not auto-reject"
    );
}

#[test]
fn prevalidation_rejection_reason_rejects_non_security_clear_error_praise() {
    let mut chunk_seen_evidence_ids: HashSet<usize> = HashSet::new();
    let used_evidence_ids: HashSet<usize> = HashSet::new();

    let suggestion = test_suggestion(
        "Missing GitHub token produces a readable error, preventing silent pull-request failures.",
    )
    .with_evidence(
        "420| let token = get_stored_token().ok_or_else(|| anyhow::anyhow!(\"GitHub token not configured\"))?;\n421| return Err(anyhow::anyhow!(\"GitHub token not configured\"));".to_string(),
    )
    .with_evidence_refs(vec![SuggestionEvidenceRef {
        snippet_id: 15,
        file: PathBuf::from("src/github.rs"),
        line: 420,
    }]);

    let reason = prevalidation_rejection_reason(
        &suggestion,
        &used_evidence_ids,
        &mut chunk_seen_evidence_ids,
    )
    .expect("non-security clear-error praise should be rejected");

    assert!(reason
        .reason
        .contains("Non-actionable behavior description"));
    assert_eq!(reason.evidence_id, Some(15));
    assert!(!reason.is_contradiction);
}

#[test]
fn prevalidation_rejection_reason_rejects_non_security_retry_praise() {
    let mut chunk_seen_evidence_ids: HashSet<usize> = HashSet::new();
    let used_evidence_ids: HashSet<usize> = HashSet::new();

    let suggestion = test_suggestion(
        "Network hiccups are automatically retried, so users rarely see hard failures.",
    )
    .with_evidence(
        "1160| let text = send_with_retry(&client, &api_key, &request).await?;\n1170| // retry on transient errors"
            .to_string(),
    )
    .with_evidence_refs(vec![SuggestionEvidenceRef {
        snippet_id: 16,
        file: PathBuf::from("src/client.rs"),
        line: 1160,
    }]);

    let reason = prevalidation_rejection_reason(
        &suggestion,
        &used_evidence_ids,
        &mut chunk_seen_evidence_ids,
    )
    .expect("non-security retry praise should be rejected");

    assert!(reason
        .reason
        .contains("Non-actionable behavior description"));
    assert_eq!(reason.evidence_id, Some(16));
    assert!(!reason.is_contradiction);
}

#[test]
fn prevalidation_non_security_praise_filter_keeps_strong_defect_risk_claims() {
    let suggestion = test_suggestion(
        "The app shows a clear error, but a race condition can still crash under load.",
    )
    .with_evidence(
        "91| return Err(anyhow::anyhow!(\"temporary error\"));\n99| // race condition near shared state"
            .to_string(),
    );

    assert!(
        deterministic_prevalidation_non_actionable_reason(&suggestion).is_none(),
        "strong defect-risk wording should not be filtered as non-actionable praise"
    );
}

#[test]
fn prevalidation_rejection_reason_rejects_internal_jargon_summary() {
    let mut chunk_seen_evidence_ids: HashSet<usize> = HashSet::new();
    let used_evidence_ids: HashSet<usize> = HashSet::new();

    let suggestion = test_suggestion("src/cache.rs line 44 fails silently in this branch.")
        .with_detail(
            "When the write call returns an error, the branch swallows it without logging, so the user sees a save success state even though data is not persisted.".to_string(),
        )
        .with_evidence(
            " 44| if let Err(_err) = cache.write(payload) {\n 45|     return Ok(());\n 46| }"
                .to_string(),
        )
        .with_evidence_refs(vec![SuggestionEvidenceRef {
            snippet_id: 19,
            file: PathBuf::from("src/cache.rs"),
            line: 44,
        }]);

    let reason = prevalidation_rejection_reason(
        &suggestion,
        &used_evidence_ids,
        &mut chunk_seen_evidence_ids,
    )
    .expect("summary with internal jargon should be rejected");

    assert!(reason.reason.contains("plain-language ethos"));
    assert!(!reason.is_contradiction);
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
fn finalize_validated_suggestions_drops_pending_without_backfill() {
    let out = finalize_validated_suggestions(vec![
        test_suggestion("v1").with_validation_state(SuggestionValidationState::Validated),
        test_suggestion("pending").with_validation_state(SuggestionValidationState::Pending),
        test_suggestion("v2").with_validation_state(SuggestionValidationState::Validated),
    ]);
    assert_eq!(out.len(), 2);
    assert!(out
        .iter()
        .all(|s| s.validation_state == SuggestionValidationState::Validated));
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
fn gate_profile_mapping_matches_expected_ranges() {
    let strict = suggestion_gate_config_for_profile(SuggestionsProfile::Strict);
    assert_eq!(strict.min_final_count, 1);
    assert_eq!(strict.max_final_count, 8);
    assert_eq!(strict.max_attempts, 3);

    let balanced = suggestion_gate_config_for_profile(SuggestionsProfile::BalancedHighVolume);
    assert_eq!(balanced.min_final_count, 1);
    assert_eq!(balanced.max_final_count, 12);
    assert_eq!(balanced.max_attempts, 4);

    let max_volume = suggestion_gate_config_for_profile(SuggestionsProfile::MaxVolume);
    assert_eq!(max_volume.min_final_count, 2);
    assert_eq!(max_volume.max_final_count, 16);
    assert_eq!(max_volume.max_attempts, 5);
}

#[test]
fn gate_snapshot_is_best_effort_when_ethos_actionable_is_below_final_count() {
    let mut config = SuggestionQualityGateConfig::default();
    config.min_final_count = 1;

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
fn gate_snapshot_ignores_count_and_time_fail_reasons_when_disabled() {
    let mut config = SuggestionQualityGateConfig::default();
    config.min_final_count = 3;
    let suggestions = vec![
        test_suggestion("one").with_validation_state(SuggestionValidationState::Validated),
        test_suggestion("two").with_validation_state(SuggestionValidationState::Validated),
    ];
    let gate = build_gate_snapshot(&config, &suggestions, config.max_suggest_ms + 1, 0.01);
    assert!(gate.passed);
    assert!(gate.fail_reasons.is_empty());
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
fn apply_readiness_filter_skips_ungrounded_backfill_candidates() {
    let suggestions = (0..9)
        .map(|i| {
            let mut suggestion = test_suggestion("Users will see broken behavior in production.")
                .with_detail("This path may fail.".to_string())
                .with_evidence(
                    " 10| const retries = 2;\n 11| let total = retries + 1;\n".to_string(),
                )
                .with_line(11)
                .with_validation_state(SuggestionValidationState::Validated);
            suggestion.file = PathBuf::from(format!("src/backfill_{}.rs", i));
            suggestion
        })
        .collect::<Vec<_>>();

    let (filtered, _dropped, _mean) =
        apply_readiness_filter(suggestions, DEFAULT_MIN_IMPLEMENTATION_READINESS_SCORE);
    assert!(filtered.is_empty());
    assert!(!filtered.iter().any(|s| {
        s.implementation_risk_flags
            .iter()
            .any(|flag| flag == "below_readiness_threshold_backfill")
    }));
}

#[test]
fn semantic_dedupe_drops_near_duplicate_topics() {
    let first =
        test_suggestion("Failed lock releases are silently ignored, leaving stale locks behind.")
            .with_detail(
                "Lock-release delete errors are hidden and can block later jobs.".to_string(),
            )
            .with_validation_state(SuggestionValidationState::Validated);

    let second =
        test_suggestion("Stale locks may remain after release failures and block future jobs.")
            .with_detail("Release lock errors are swallowed without logging.".to_string())
            .with_validation_state(SuggestionValidationState::Validated);

    let third =
        test_suggestion("Adding users to the email audience can fail, causing missed alerts.")
            .with_detail(
                "Audience add failures increment errors and users are skipped for this sync."
                    .to_string(),
            )
            .with_validation_state(SuggestionValidationState::Validated);

    let (deduped, dropped) = semantic_dedupe_validated_suggestions(vec![first, second, third]);
    assert_eq!(dropped, 1);
    assert_eq!(deduped.len(), 2);

    let diversity = compute_suggestion_diversity_metrics(&deduped);
    assert!(diversity.unique_topic_count >= 2);
    assert!(diversity.dominant_topic_ratio <= 0.5);
}

#[test]
fn file_balance_caps_dominant_file_when_alternatives_exist() {
    let mut suggestions = Vec::new();
    for i in 0..5 {
        let mut s = test_suggestion(&format!("Primary flow issue {}", i))
            .with_validation_state(SuggestionValidationState::Validated);
        s.file = PathBuf::from("src/primary.ts");
        suggestions.push(s);
    }
    for i in 0..5 {
        let mut s = test_suggestion(&format!("Secondary flow issue {}", i))
            .with_validation_state(SuggestionValidationState::Validated);
        s.file = PathBuf::from(format!("src/secondary_{}.ts", i));
        suggestions.push(s);
    }

    let (balanced, dropped) = balance_suggestions_across_files(suggestions, 3, 8);
    assert_eq!(balanced.len(), 8);
    assert_eq!(dropped, 2);
    let dominant_file_count = balanced
        .iter()
        .filter(|s| s.file == Path::new("src/primary.ts"))
        .count();
    assert_eq!(dominant_file_count, 3);
}

#[test]
fn gate_snapshot_keeps_diversity_metrics_without_enforcing_file_gate() {
    let mut config = SuggestionQualityGateConfig::default();
    config.min_final_count = 4;
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
    assert!(gate.dominant_file_ratio > DIVERSITY_DOMINANT_FILE_RATIO_MAX);
    assert!(gate.unique_file_count < DIVERSITY_MIN_UNIQUE_FILES);
}
