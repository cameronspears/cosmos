use super::*;
use chrono::Utc;
use cosmos_core::context::WorkContext;
use cosmos_core::index::{
    CodebaseIndex, FileIndex, FileSummary, Language, Pattern, PatternKind, PatternReliability,
    Symbol, SymbolKind, Visibility,
};
use cosmos_core::suggest::{Priority, SuggestionKind, SuggestionSource};
use serde_json::json;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

fn test_suggestion(summary: &str) -> Suggestion {
    Suggestion::new(
        SuggestionKind::Improvement,
        Priority::Medium,
        std::path::PathBuf::from("src/lib.rs"),
        summary.to_string(),
        SuggestionSource::LlmDeep,
    )
}

fn test_evidence_item(id: usize) -> EvidenceItem {
    EvidenceItem {
        id,
        file: PathBuf::from(format!("src/file_{}.rs", id)),
        line: id + 1,
        snippet: format!("{}| let value = {};", id + 1, id),
        why_interesting: "test".to_string(),
        source: EvidenceSource::Pattern,
        pattern_kind: None,
    }
}

#[test]
fn redacts_secret_like_tokens_from_snippets() {
    let snippet = r#"  10| const API_KEY = "sk-1234567890abcdefghijkl";
  11| authorization = "Bearer ghp_abcdefghijklmnopqrstuvwxyz123456";
  12| password = "super-secret-value";
"#;
    let redacted = redact_obvious_secrets(snippet);
    assert!(!redacted.contains("sk-1234567890abcdefghijkl"));
    assert!(!redacted.contains("ghp_abcdefghijklmnopqrstuvwxyz123456"));
    assert!(!redacted.contains("super-secret-value"));
    assert!(redacted.contains("<redacted-secret>"));
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
fn parses_legacy_top_level_evidence_id_shape() {
    let parsed: FastGroundedSuggestionJson = serde_json::from_value(json!({
        "evidence_id": 7,
        "kind": "bugfix",
        "priority": "high",
        "confidence": "high",
        "summary": "Legacy shape",
        "detail": "Still supported"
    }))
    .expect("legacy shape should deserialize");

    assert_eq!(parsed.evidence_id, Some(7));
    assert!(parsed.evidence_refs.is_empty());
}

#[test]
fn parses_mixed_evidence_refs_shapes() {
    let parsed: FastGroundedSuggestionJson = serde_json::from_value(json!({
        "evidence_refs": [1, "2", {"evidence_id": 3}],
        "kind": "improvement",
        "priority": "medium",
        "confidence": "medium",
        "summary": "Mixed shape",
        "detail": "Accepted for robustness"
    }))
    .expect("mixed evidence_refs shape should deserialize");

    assert!(matches!(
        parsed.evidence_refs[0],
        FastGroundedEvidenceRefJson::Integer(1)
    ));
    assert!(matches!(
        parsed.evidence_refs[1],
        FastGroundedEvidenceRefJson::String(ref raw) if raw == "2"
    ));
    assert!(matches!(
        parsed.evidence_refs[2],
        FastGroundedEvidenceRefJson::Object {
            evidence_id: Some(3),
            ..
        }
    ));
}

#[test]
fn parses_object_evidence_ref_with_snippet_id() {
    let parsed: FastGroundedSuggestionJson = serde_json::from_value(json!({
        "evidence_refs": [{
            "snippet_id": 5,
            "file": "src/main.rs",
            "line": 42
        }],
        "kind": "reliability",
        "priority": "high",
        "confidence": "medium",
        "summary": "Object shape",
        "detail": "Should deserialize robustly"
    }))
    .expect("object evidence ref shape should deserialize");

    match &parsed.evidence_refs[0] {
        FastGroundedEvidenceRefJson::Object {
            evidence_id,
            snippet_id,
        } => {
            assert_eq!(*evidence_id, None);
            assert_eq!(*snippet_id, Some(5));
        }
        _ => panic!("expected object evidence ref"),
    }
}

#[test]
fn collect_valid_evidence_refs_accepts_numeric_string_id() {
    let pack = vec![test_evidence_item(0), test_evidence_item(4)];
    let suggestion = FastGroundedSuggestionJson {
        evidence_refs: vec![FastGroundedEvidenceRefJson::String("4".to_string())],
        evidence_id: None,
        snippet_id: None,
        kind: "improvement".to_string(),
        priority: "medium".to_string(),
        confidence: "medium".to_string(),
        summary: "test".to_string(),
        detail: "detail".to_string(),
    };

    let refs = collect_valid_evidence_refs(&suggestion, &pack);
    assert_eq!(refs.len(), 1);
    assert_eq!(refs[0].snippet_id, 4);
}

#[test]
fn grounded_finalizer_does_not_backfill_duplicates() {
    let mapped = vec![
        (1, test_suggestion("a")),
        (1, test_suggestion("a-duplicate")),
        (2, test_suggestion("b")),
        (2, test_suggestion("b-duplicate")),
    ];

    let result = dedupe_and_cap_grounded_suggestions(mapped, FAST_GROUNDED_PROVISIONAL_TARGET_MAX);

    assert_eq!(result.len(), 2);
}

#[test]
fn grounded_finalizer_caps_results_at_provisional_target_max() {
    let mapped: Vec<(usize, Suggestion)> = (0..40)
        .map(|i| (i, test_suggestion(&format!("item-{}", i))))
        .collect();

    let result = dedupe_and_cap_grounded_suggestions(mapped, FAST_GROUNDED_PROVISIONAL_TARGET_MAX);

    assert_eq!(result.len(), FAST_GROUNDED_PROVISIONAL_TARGET_MAX);
}

#[test]
fn build_evidence_pack_is_deterministic_with_tie_breakers() {
    let root = temp_root("deterministic");
    write_fixture_file(&root, "src/a.rs", 80);
    write_fixture_file(&root, "src/b.rs", 80);
    write_fixture_file(&root, "src/c.rs", 80);

    let mut files = HashMap::new();
    for rel in ["src/a.rs", "src/b.rs", "src/c.rs"] {
        let pattern = Pattern {
            kind: PatternKind::MissingErrorHandling,
            file: PathBuf::from(rel),
            line: 12,
            description: "Unchecked unwrap".to_string(),
            reliability: PatternReliability::High,
        };
        let symbol = Symbol {
            name: "handle".to_string(),
            kind: SymbolKind::Function,
            file: PathBuf::from(rel),
            line: 12,
            end_line: 30,
            complexity: 12.0,
            visibility: Visibility::Public,
        };
        let (path, index) = mk_file_index(rel, 120, 30.0, vec![pattern], vec![symbol], 3);
        files.insert(path, index);
    }
    let index = CodebaseIndex {
        root: root.clone(),
        files,
        index_errors: Vec::new(),
        git_head: None,
    };
    let context = empty_context(&root);

    let (pack_a, _) = build_evidence_pack(&root, &index, &context);
    let (pack_b, _) = build_evidence_pack(&root, &index, &context);

    let ids_a: Vec<_> = pack_a.iter().map(|i| (i.file.clone(), i.line)).collect();
    let ids_b: Vec<_> = pack_b.iter().map(|i| (i.file.clone(), i.line)).collect();
    assert_eq!(ids_a, ids_b);

    let first_paths: Vec<_> = pack_a
        .iter()
        .take(3)
        .map(|i| i.file.display().to_string())
        .collect();
    assert!(first_paths.windows(2).all(|w| w[0] <= w[1]));

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn build_evidence_pack_enforces_source_and_godmodule_quotas() {
    let root = temp_root("quotas");
    let mut files = HashMap::new();

    for i in 0..24 {
        let rel = format!("src/f{}.rs", i);
        write_fixture_file(&root, &rel, 120);
        let pattern = Pattern {
            kind: PatternKind::GodModule,
            file: PathBuf::from(&rel),
            line: 1,
            description: "Large module".to_string(),
            reliability: PatternReliability::Low,
        };
        let symbol = Symbol {
            name: format!("work_{}", i),
            kind: SymbolKind::Function,
            file: PathBuf::from(&rel),
            line: 40,
            end_line: 70,
            complexity: 20.0 + i as f64,
            visibility: Visibility::Public,
        };
        let (path, index) =
            mk_file_index(&rel, 900, 40.0 + i as f64, vec![pattern], vec![symbol], 4);
        files.insert(path, index);
    }

    let index = CodebaseIndex {
        root: root.clone(),
        files,
        index_errors: Vec::new(),
        git_head: None,
    };
    let context = empty_context(&root);
    let (pack, stats) = build_evidence_pack(&root, &index, &context);

    let godmodule_count = pack
        .iter()
        .filter(|item| item.pattern_kind == Some(PatternKind::GodModule))
        .count();
    assert!(stats.pattern_count <= FAST_EVIDENCE_SOURCE_PATTERN_MAX);
    assert!(godmodule_count <= FAST_EVIDENCE_KIND_GOD_MODULE_MAX);
    assert!(pack.len() <= FAST_EVIDENCE_PACK_MAX_ITEMS);
    assert!(stats.line1_ratio <= 0.5);

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn godmodule_anchor_prefers_complex_function_line_not_line1() {
    let root = temp_root("godmodule_anchor");
    let rel = "src/module.rs";
    write_fixture_file(&root, rel, 140);

    let pattern = Pattern {
        kind: PatternKind::GodModule,
        file: PathBuf::from(rel),
        line: 1,
        description: "File has many lines".to_string(),
        reliability: PatternReliability::Low,
    };
    let symbol = Symbol {
        name: "critical_path".to_string(),
        kind: SymbolKind::Function,
        file: PathBuf::from(rel),
        line: 72,
        end_line: 110,
        complexity: 88.0,
        visibility: Visibility::Public,
    };
    let (path, index_file) = mk_file_index(rel, 300, 10.0, vec![pattern], vec![symbol], 0);
    let mut files = HashMap::new();
    files.insert(path, index_file);
    let index = CodebaseIndex {
        root: root.clone(),
        files,
        index_errors: Vec::new(),
        git_head: None,
    };
    let context = empty_context(&root);
    let (pack, _) = build_evidence_pack(&root, &index, &context);

    let godmodule_item = pack
        .iter()
        .find(|item| item.pattern_kind == Some(PatternKind::GodModule))
        .expect("expected godmodule evidence item");
    assert_eq!(godmodule_item.line, 72);

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn regeneration_needed_uses_soft_floor_ten() {
    assert_eq!(regeneration_needed(0), 10);
    assert_eq!(regeneration_needed(1), 9);
    assert_eq!(regeneration_needed(9), 1);
    assert_eq!(regeneration_needed(10), 0);
    assert_eq!(regeneration_needed(14), 0);
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
fn should_run_mapping_rescue_only_when_raw_exists_and_mapped_is_empty() {
    assert!(should_run_mapping_rescue(3, 0));
    assert!(!should_run_mapping_rescue(0, 0));
    assert!(!should_run_mapping_rescue(3, 1));
}

#[test]
fn generation_topup_decision_is_based_on_mapped_count_and_call_budget() {
    assert!(should_run_generation_topup(
        FAST_GROUNDED_VALIDATED_HARD_TARGET - 1,
        0,
        0,
        SUGGEST_BALANCED_BUDGET_MS
    ));
    assert!(!should_run_generation_topup(
        FAST_GROUNDED_VALIDATED_HARD_TARGET,
        0,
        0,
        SUGGEST_BALANCED_BUDGET_MS
    ));
    assert!(!should_run_generation_topup(
        0,
        GENERATION_TOPUP_MAX_CALLS,
        0,
        SUGGEST_BALANCED_BUDGET_MS
    ));
}

#[test]
fn generation_topup_request_count_uses_deficit_plus_padding_with_cap() {
    assert_eq!(generation_topup_request_count(1), 4);
    assert_eq!(generation_topup_request_count(6), 9);
    assert_eq!(generation_topup_request_count(20), 10);
}

#[test]
fn generation_topup_requires_remaining_budget() {
    assert!(should_run_generation_topup(
        FAST_GROUNDED_VALIDATED_HARD_TARGET - 1,
        0,
        0,
        SUGGEST_BALANCED_BUDGET_MS
    ));
    assert!(!should_run_generation_topup(
        FAST_GROUNDED_VALIDATED_HARD_TARGET - 1,
        0,
        SUGGEST_BALANCED_BUDGET_MS - GENERATION_TOPUP_TIMEOUT_MS + 1,
        SUGGEST_BALANCED_BUDGET_MS
    ));
}

#[test]
fn regeneration_request_bounds_scale_and_clamp_to_range() {
    assert_eq!(regeneration_request_bounds(1), (4, 4));
    assert_eq!(regeneration_request_bounds(2), (4, 6));
    assert_eq!(regeneration_request_bounds(4), (8, 12));
    assert_eq!(regeneration_request_bounds(5), (10, 14));
    assert_eq!(regeneration_request_bounds(10), (12, 14));
}

#[test]
fn regeneration_needed_for_target_uses_target_count() {
    assert_eq!(regeneration_needed_for_target(0, 15), 15);
    assert_eq!(regeneration_needed_for_target(10, 15), 5);
    assert_eq!(regeneration_needed_for_target(15, 15), 0);
    assert_eq!(regeneration_needed_for_target(18, 15), 0);
}

#[test]
fn choose_regeneration_phase_target_prioritizes_hard_then_stretch_target() {
    assert_eq!(
        choose_regeneration_phase_target(
            9,
            FAST_GROUNDED_VALIDATED_HARD_TARGET,
            FAST_GROUNDED_VALIDATED_STRETCH_TARGET,
            0,
            0
        ),
        Some(FAST_GROUNDED_VALIDATED_HARD_TARGET)
    );
    assert_eq!(
        choose_regeneration_phase_target(
            9,
            FAST_GROUNDED_VALIDATED_HARD_TARGET,
            FAST_GROUNDED_VALIDATED_STRETCH_TARGET,
            REFINEMENT_HARD_PHASE_MAX_ATTEMPTS,
            0
        ),
        None
    );
    assert_eq!(
        choose_regeneration_phase_target(
            FAST_GROUNDED_VALIDATED_HARD_TARGET,
            FAST_GROUNDED_VALIDATED_HARD_TARGET,
            FAST_GROUNDED_VALIDATED_STRETCH_TARGET,
            REFINEMENT_HARD_PHASE_MAX_ATTEMPTS,
            0
        ),
        Some(FAST_GROUNDED_VALIDATED_STRETCH_TARGET)
    );
    assert_eq!(
        choose_regeneration_phase_target(
            FAST_GROUNDED_VALIDATED_HARD_TARGET,
            FAST_GROUNDED_VALIDATED_HARD_TARGET,
            FAST_GROUNDED_VALIDATED_STRETCH_TARGET,
            REFINEMENT_HARD_PHASE_MAX_ATTEMPTS,
            REFINEMENT_STRETCH_PHASE_MAX_ATTEMPTS
        ),
        None
    );
    assert_eq!(
        choose_regeneration_phase_target(
            FAST_GROUNDED_VALIDATED_STRETCH_TARGET,
            FAST_GROUNDED_VALIDATED_HARD_TARGET,
            FAST_GROUNDED_VALIDATED_STRETCH_TARGET,
            0,
            0
        ),
        None
    );
}

#[test]
fn build_remaining_pack_excludes_rejected_evidence_when_strict_pack_is_large_enough() {
    let pack = (0..8).map(test_evidence_item).collect::<Vec<_>>();
    let used = HashSet::from([0usize]);
    let rejected = HashSet::from([1usize, 2usize]);

    let (remaining, used_relaxed_filter, skipped_rejected_ids) =
        build_remaining_pack_for_regeneration(&pack, &used, &rejected, false);

    assert!(!used_relaxed_filter);
    let remaining_ids = remaining.iter().map(|item| item.id).collect::<Vec<_>>();
    assert_eq!(remaining_ids, vec![3, 4, 5, 6, 7]);
    assert_eq!(skipped_rejected_ids, vec![1, 2]);
}

#[test]
fn build_remaining_pack_relaxes_rejected_filter_once_when_strict_pack_is_too_small() {
    let pack = (0..8).map(test_evidence_item).collect::<Vec<_>>();
    let used = HashSet::from([0usize, 6usize, 7usize]);
    let rejected = HashSet::from([1usize, 2usize, 3usize]);

    let (remaining, used_relaxed_filter, skipped_rejected_ids) =
        build_remaining_pack_for_regeneration(&pack, &used, &rejected, true);

    assert!(used_relaxed_filter);
    let remaining_ids = remaining.iter().map(|item| item.id).collect::<Vec<_>>();
    assert_eq!(remaining_ids, vec![1, 2, 3, 4, 5]);
    assert!(skipped_rejected_ids.is_empty());
}

#[test]
fn sort_validation_outcomes_restores_input_order_for_parallel_results() {
    let mut outcomes: Vec<ValidationOutcome> = vec![
        (
            2,
            test_suggestion("c"),
            0,
            SuggestionValidationState::Validated,
            "ok".to_string(),
            None,
            None,
        ),
        (
            0,
            test_suggestion("a"),
            0,
            SuggestionValidationState::Validated,
            "ok".to_string(),
            None,
            None,
        ),
        (
            1,
            test_suggestion("b"),
            0,
            SuggestionValidationState::Rejected,
            "no".to_string(),
            None,
            Some(ValidationRejectClass::Other),
        ),
    ];
    sort_validation_outcomes(&mut outcomes);
    let summaries = outcomes
        .iter()
        .map(|(_, suggestion, _, _, _, _, _)| suggestion.summary.clone())
        .collect::<Vec<_>>();
    assert_eq!(summaries, vec!["a", "b", "c"]);
}

#[test]
fn should_stop_regeneration_for_validation_budget_blocks_deadline_or_low_budget() {
    assert!(should_stop_regeneration_for_validation_budget(true, 10_000));
    assert!(should_stop_regeneration_for_validation_budget(
        false,
        VALIDATION_MIN_REMAINING_BUDGET_MS - 1
    ));
    assert!(!should_stop_regeneration_for_validation_budget(
        false,
        VALIDATION_MIN_REMAINING_BUDGET_MS
    ));
}

#[test]
fn should_retry_transport_rejection_allows_single_retry_with_time_remaining() {
    let future_deadline = std::time::Instant::now()
        + std::time::Duration::from_millis(VALIDATION_RETRY_MIN_REMAINING_BUDGET_MS + 200);
    let near_deadline = std::time::Instant::now()
        + std::time::Duration::from_millis(VALIDATION_RETRY_MIN_REMAINING_BUDGET_MS - 100);
    let past_deadline = std::time::Instant::now() - std::time::Duration::from_millis(1);
    assert!(should_retry_transport_rejection(
        ValidationRejectClass::Transport,
        0,
        future_deadline
    ));
    assert!(!should_retry_transport_rejection(
        ValidationRejectClass::Transport,
        VALIDATION_RETRY_MAX_PER_SUGGESTION,
        future_deadline
    ));
    assert!(!should_retry_transport_rejection(
        ValidationRejectClass::Contradicted,
        0,
        future_deadline
    ));
    assert!(!should_retry_transport_rejection(
        ValidationRejectClass::Transport,
        0,
        past_deadline
    ));
    assert!(!should_retry_transport_rejection(
        ValidationRejectClass::Transport,
        0,
        near_deadline
    ));
}

#[test]
fn prevalidation_rejection_reason_catches_missing_and_duplicate_primary_evidence() {
    let mut chunk_seen_evidence_ids: HashSet<usize> = HashSet::new();
    let used_evidence_ids: HashSet<usize> = HashSet::new();

    let missing = test_suggestion("missing refs");
    let missing_reason =
        prevalidation_rejection_reason(&missing, &used_evidence_ids, &mut chunk_seen_evidence_ids)
            .expect("missing evidence should be rejected");
    assert!(missing_reason.0.contains("Missing primary evidence ref"));
    assert!(missing_reason.1.is_none());

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
        .0
        .contains("Duplicate evidence_id in validation batch"));
    assert_eq!(duplicate_reason.1, Some(3));

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
        .0
        .contains("Duplicate evidence_id already validated"));
    assert_eq!(duplicate_used_reason.1, Some(9));
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

    assert!(reason.0.contains("client ID appears configured"));
    assert_eq!(reason.1, Some(11));
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

    assert!(reason.0.contains("absolute-path security guard"));
    assert_eq!(reason.1, Some(12));
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

    assert!(reason.0.contains("cache-directory creation/ensure logic"));
    assert_eq!(reason.1, Some(13));
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

    assert!(reason.0.contains("Non-actionable safeguard description"));
    assert_eq!(reason.1, Some(14));
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

    assert!(reason.0.contains("Non-actionable behavior description"));
    assert_eq!(reason.1, Some(15));
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

    assert!(reason.0.contains("Non-actionable behavior description"));
    assert_eq!(reason.1, Some(16));
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
fn remap_suggestion_to_original_ids_handles_non_contiguous_ids() {
    let full_pack = vec![
        EvidenceItem {
            id: 10,
            file: PathBuf::from("src/a.rs"),
            line: 7,
            snippet: "7| let a = 1;".to_string(),
            why_interesting: "pattern".to_string(),
            source: EvidenceSource::Pattern,
            pattern_kind: Some(PatternKind::MissingErrorHandling),
        },
        EvidenceItem {
            id: 42,
            file: PathBuf::from("src/b.rs"),
            line: 11,
            snippet: "11| let b = 2;".to_string(),
            why_interesting: "hotspot".to_string(),
            source: EvidenceSource::Hotspot,
            pattern_kind: None,
        },
    ];
    let (local_pack, local_to_original) = renumber_pack(&full_pack);
    assert_eq!(local_pack[0].id, 0);
    assert_eq!(local_pack[1].id, 1);

    let mut suggestion = test_suggestion("local-id")
        .with_line(local_pack[1].line)
        .with_evidence(local_pack[1].snippet.clone())
        .with_evidence_refs(vec![SuggestionEvidenceRef {
            snippet_id: 1,
            file: local_pack[1].file.clone(),
            line: local_pack[1].line,
        }]);

    assert!(remap_suggestion_to_original_ids(
        &mut suggestion,
        &local_to_original,
        &full_pack
    ));
    assert_eq!(suggestion.evidence_refs[0].snippet_id, 42);
    assert_eq!(suggestion.file, PathBuf::from("src/b.rs"));
    assert_eq!(suggestion.line, Some(11));
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
fn grounded_schema_enforces_single_evidence_ref() {
    let schema = grounded_suggestion_schema(10);
    let evidence_refs =
        &schema["properties"]["suggestions"]["items"]["properties"]["evidence_refs"];
    let evidence_id = &evidence_refs["items"]["properties"]["evidence_id"];
    assert!(evidence_refs.get("minItems").is_none());
    assert!(evidence_refs.get("maxItems").is_none());
    assert_eq!(evidence_id.get("minimum").and_then(|v| v.as_u64()), Some(0));
    assert_eq!(evidence_id.get("maximum").and_then(|v| v.as_u64()), Some(9));
}

#[test]
fn collect_valid_evidence_refs_truncates_to_one_ref() {
    let pack = vec![test_evidence_item(0), test_evidence_item(1)];
    let suggestion = FastGroundedSuggestionJson {
        evidence_refs: vec![
            FastGroundedEvidenceRefJson::Integer(0),
            FastGroundedEvidenceRefJson::Integer(1),
        ],
        evidence_id: None,
        snippet_id: None,
        kind: "improvement".to_string(),
        priority: "medium".to_string(),
        confidence: "medium".to_string(),
        summary: "test".to_string(),
        detail: "detail".to_string(),
    };

    let refs = collect_valid_evidence_refs(&suggestion, &pack);
    assert_eq!(refs.len(), 1);
    assert_eq!(refs[0].snippet_id, 0);
}

#[test]
fn suggestion_batch_validation_schema_sets_local_index_bounds() {
    let schema = suggestion_batch_validation_schema(5);
    let local_index = &schema["properties"]["validations"]["items"]["properties"]["local_index"];
    assert_eq!(local_index.get("minimum").and_then(|v| v.as_u64()), Some(0));
    assert_eq!(local_index.get("maximum").and_then(|v| v.as_u64()), Some(5));
}

#[test]
fn map_batch_validation_response_fills_missing_entries() {
    let mapped = map_batch_validation_response(
        3,
        SuggestionBatchValidationJson {
            validations: vec![
                SuggestionBatchValidationItemJson {
                    local_index: 1,
                    validation: "validated".to_string(),
                    reason: "supported by snippet".to_string(),
                },
                SuggestionBatchValidationItemJson {
                    local_index: 2,
                    validation: "unexpected".to_string(),
                    reason: String::new(),
                },
                SuggestionBatchValidationItemJson {
                    local_index: 9,
                    validation: "validated".to_string(),
                    reason: "ignored out of range".to_string(),
                },
            ],
        },
    );

    assert_eq!(mapped.len(), 3);

    let (state0, reason0, class0) = &mapped[0];
    assert_eq!(*state0, SuggestionValidationState::Rejected);
    assert!(reason0.contains("missing batch result"));
    assert!(matches!(class0, Some(ValidationRejectClass::Transport)));

    let (state1, _reason1, class1) = &mapped[1];
    assert_eq!(*state1, SuggestionValidationState::Validated);
    assert!(class1.is_none());

    let (state2, reason2, class2) = &mapped[2];
    assert_eq!(*state2, SuggestionValidationState::Rejected);
    assert!(reason2.contains("no reason"));
    assert!(matches!(class2, Some(ValidationRejectClass::Other)));
}

#[test]
fn gate_snapshot_reports_fail_reasons_for_count_and_cost() {
    let config = SuggestionQualityGateConfig::default();
    let suggestions = vec![
        test_suggestion("one").with_validation_state(SuggestionValidationState::Validated),
        test_suggestion("two").with_validation_state(SuggestionValidationState::Validated),
    ];
    let gate = build_gate_snapshot(&config, &suggestions, 3_000, 0.04);
    assert!(!gate.passed);
    assert!(gate
        .fail_reasons
        .iter()
        .any(|reason| reason.contains("final_count")));
    assert!(gate
        .fail_reasons
        .iter()
        .any(|reason| reason.contains("suggest_total_cost_usd")));
}

#[test]
fn gate_snapshot_prefers_higher_validity_and_count() {
    let better = SuggestionGateSnapshot {
        final_count: 12,
        displayed_valid_ratio: 1.0,
        pending_count: 0,
        suggest_total_ms: 20_000,
        suggest_total_cost_usd: 0.012,
        dominant_topic_ratio: 0.40,
        unique_topic_count: 6,
        dominant_file_ratio: 0.40,
        unique_file_count: 6,
        passed: true,
        fail_reasons: Vec::new(),
    };
    let worse = SuggestionGateSnapshot {
        final_count: 8,
        displayed_valid_ratio: 0.9,
        pending_count: 0,
        suggest_total_ms: 15_000,
        suggest_total_cost_usd: 0.010,
        dominant_topic_ratio: 0.90,
        unique_topic_count: 1,
        dominant_file_ratio: 0.90,
        unique_file_count: 1,
        passed: false,
        fail_reasons: vec!["count".to_string()],
    };
    assert!(gate_snapshot_is_better(&better, &worse));
    assert!(!gate_snapshot_is_better(&worse, &better));
}

#[test]
fn overclaim_reason_detector_matches_expected_markers() {
    assert!(is_overclaim_validation_reason(
        "Suggestion makes assumptions beyond evidence about business impact"
    ));
    assert!(is_overclaim_validation_reason(
        "Claims UI behavior without proof from snippet"
    ));
    assert!(!is_overclaim_validation_reason(
        "Validation failed: deadline exceeded"
    ));
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
fn apply_readiness_filter_backfills_when_threshold_is_too_strict() {
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
    let expected_floor = FAST_GROUNDED_FINAL_TARGET_MIN.saturating_sub(2);
    assert_eq!(filtered.len(), expected_floor);
    assert!(filtered.iter().any(|s| {
        s.implementation_risk_flags
            .iter()
            .any(|flag| flag == "below_readiness_threshold_backfill")
    }));
}

#[test]
fn speculative_filter_keeps_grounded_crash_claim_when_evidence_proves_it() {
    let suggestion = test_suggestion("The process can crash when this panic path executes.")
        .with_detail("A panic path is present and unhandled in this flow.".to_string())
        .with_evidence(
            " 41| if config.invalid() {\n 42|     panic!(\"invalid config\");\n 43| }\n"
                .to_string(),
        )
        .with_evidence_refs(vec![SuggestionEvidenceRef {
            snippet_id: 11,
            file: PathBuf::from("src/runtime.rs"),
            line: 42,
        }]);

    let (kept, dropped) = filter_speculative_impact_suggestions(vec![suggestion]);
    assert_eq!(dropped, 0);
    assert_eq!(kept.len(), 1);
}

#[test]
fn speculative_filter_drops_unsupported_business_impact_claims() {
    let suggestion =
        test_suggestion("This path can hurt campaign reach and outreach effectiveness for teams.")
            .with_detail("The function retries once and then returns false.".to_string())
            .with_evidence(
                " 12| let ok = retry_once();\n 13| if !ok {\n 14|   return false;\n 15| }\n"
                    .to_string(),
            )
            .with_evidence_refs(vec![SuggestionEvidenceRef {
                snippet_id: 12,
                file: PathBuf::from("src/worker.rs"),
                line: 14,
            }]);

    let (kept, dropped) = filter_speculative_impact_suggestions(vec![suggestion]);
    assert_eq!(kept.len(), 0);
    assert_eq!(dropped, 1);
}

#[test]
fn deterministic_auto_validation_accepts_empty_catch_with_silent_error_language() {
    let suggestion = test_suggestion("Errors are silently ignored in this flow.")
        .with_detail("A catch block is empty, so failures are not logged.".to_string())
        .with_evidence(
            " 10| try {\n 11|   runTask();\n 12| } catch (error) {\n 13| }\n".to_string(),
        )
        .with_evidence_refs(vec![SuggestionEvidenceRef {
            snippet_id: 7,
            file: PathBuf::from("src/a.ts"),
            line: 12,
        }]);

    let reason = deterministic_auto_validation_reason(&suggestion);
    assert!(reason.is_some());
}

#[test]
fn deterministic_auto_validation_rejects_non_empty_catch() {
    let suggestion = test_suggestion("Errors are silently ignored in this flow.")
            .with_detail("A catch block is empty, so failures are not logged.".to_string())
            .with_evidence(
                " 10| try {\n 11|   runTask();\n 12| } catch (error) {\n 13|   console.error(error);\n 14| }\n".to_string(),
            )
            .with_evidence_refs(vec![SuggestionEvidenceRef {
                snippet_id: 7,
                file: PathBuf::from("src/a.ts"),
                line: 12,
            }]);

    let reason = deterministic_auto_validation_reason(&suggestion);
    assert!(reason.is_none());
}

#[test]
fn deterministic_auto_validation_rejects_unanchored_impact_claims() {
    let suggestion =
        test_suggestion("Failed lock releases can leave stale locks that block future jobs.")
            .with_detail("A catch block is empty, so lock cleanup failures are hidden.".to_string())
            .with_evidence(
                " 10| try {\n 11|   runTask();\n 12| } catch (error) {\n 13| }\n".to_string(),
            )
            .with_evidence_refs(vec![SuggestionEvidenceRef {
                snippet_id: 9,
                file: PathBuf::from("src/a.ts"),
                line: 12,
            }]);

    let reason = deterministic_auto_validation_reason(&suggestion);
    assert!(reason.is_none());
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
fn speculative_impact_filter_rewrites_ungrounded_memory_claims() {
    let speculative = test_suggestion(
            "Leaked observers may cause memory growth, slowing the browser over time.",
        )
        .with_detail("Disconnect errors are ignored in cleanup.".to_string())
        .with_evidence(
            " 10| const po = new PerformanceObserver(() => {});\n 11| try {\n 12|   po.disconnect();\n 13| } catch {}\n"
                .to_string(),
        )
        .with_validation_state(SuggestionValidationState::Validated);
    let grounded =
        test_suggestion("Metric updates can fail silently, so monitoring data is missing.")
            .with_detail("Empty catch blocks can suppress metric errors.".to_string())
            .with_evidence(" 20| try {\n 21|   sendMetric();\n 22| } catch {}\n".to_string())
            .with_validation_state(SuggestionValidationState::Validated);

    let (filtered, dropped) =
        filter_speculative_impact_suggestions(vec![speculative, grounded.clone()]);
    assert_eq!(dropped, 0);
    assert_eq!(filtered.len(), 2);
    let rewritten = filtered
        .iter()
        .find(|s| s.summary.to_ascii_lowercase().contains("telemetry"))
        .expect("expected conservative telemetry rewrite");
    assert!(!rewritten
        .summary
        .to_ascii_lowercase()
        .contains("memory growth"));
}

#[test]
fn speculative_impact_filter_rewrites_audience_claims_to_data_drift() {
    let audience = test_suggestion(
            "Users may miss important alerts because audience updates fail, reducing campaign reach.",
        )
        .with_detail("Audience set writes are best-effort.".to_string())
        .with_evidence(
            " 50| try {\n 51|   await redis.sadd(DUMP_ALERT_AUDIENCE_SET, userEmail);\n 52| } catch {}\n"
                .to_string(),
        )
        .with_validation_state(SuggestionValidationState::Validated);

    let (filtered, dropped) = filter_speculative_impact_suggestions(vec![audience]);
    assert_eq!(dropped, 0);
    assert_eq!(filtered.len(), 1);
    let summary = filtered[0].summary.to_ascii_lowercase();
    assert!(summary.contains("audience"));
    assert!(summary.contains("drift"));
    assert!(!summary.contains("campaign reach"));
}

#[test]
fn gate_snapshot_fails_when_file_concentration_is_too_high() {
    let config = SuggestionQualityGateConfig::default();
    let mut suggestions = Vec::new();
    for i in 0..config.min_final_count {
        let mut suggestion = test_suggestion(&format!("Distinct issue {}", i))
            .with_validation_state(SuggestionValidationState::Validated);
        suggestion.file = PathBuf::from("src/one_file.ts");
        suggestions.push(suggestion);
    }

    let gate = build_gate_snapshot(&config, &suggestions, 10_000, 0.01);
    assert!(!gate.passed);
    assert!(gate
        .fail_reasons
        .iter()
        .any(|reason| reason.starts_with("dominant_file_ratio")));
    assert!(gate
        .fail_reasons
        .iter()
        .any(|reason| reason.starts_with("unique_file_count")));
}

#[test]
fn parse_validation_state_accepts_common_synonyms() {
    let (state, class) = parse_validation_state("supported_by_evidence");
    assert_eq!(state, SuggestionValidationState::Validated);
    assert!(class.is_none());

    let (state, class) = parse_validation_state("insufficient evidence");
    assert_eq!(state, SuggestionValidationState::Rejected);
    assert_eq!(class, Some(ValidationRejectClass::InsufficientEvidence));

    let (state, class) = parse_validation_state("not supported");
    assert_eq!(state, SuggestionValidationState::Rejected);
    assert_eq!(class, Some(ValidationRejectClass::Contradicted));
}

#[test]
fn reconcile_validation_from_reason_recovers_supported_other_label() {
    let (state, class) = reconcile_validation_from_reason(
        SuggestionValidationState::Rejected,
        Some(ValidationRejectClass::Other),
        "Evidence contains an empty catch block, confirming this suggestion is supported.",
    );
    assert_eq!(state, SuggestionValidationState::Validated);
    assert!(class.is_none());
}

#[test]
fn should_retry_after_gate_miss_skips_cost_only_misses() {
    let config = SuggestionQualityGateConfig::default();
    let gate = SuggestionGateSnapshot {
        final_count: config.min_final_count + 1,
        displayed_valid_ratio: config.min_displayed_valid_ratio,
        pending_count: 0,
        suggest_total_ms: config.max_suggest_ms + 100,
        suggest_total_cost_usd: config.max_suggest_cost_usd + 0.001,
        dominant_topic_ratio: 0.30,
        unique_topic_count: config.min_final_count,
        dominant_file_ratio: 0.30,
        unique_file_count: config.min_final_count,
        passed: false,
        fail_reasons: vec!["cost".to_string(), "latency".to_string()],
    };
    assert!(!should_retry_after_gate_miss(
        &config,
        &gate,
        config.max_suggest_cost_usd * 0.95,
        GATE_RETRY_MIN_REMAINING_BUDGET_MS + 1
    ));
}

#[test]
fn choose_regeneration_phase_target_returns_stretch_after_hard_is_met() {
    let selected = choose_regeneration_phase_target(
        FAST_GROUNDED_VALIDATED_HARD_TARGET,
        FAST_GROUNDED_VALIDATED_HARD_TARGET,
        FAST_GROUNDED_VALIDATED_STRETCH_TARGET,
        REFINEMENT_HARD_PHASE_MAX_ATTEMPTS,
        0,
    );
    assert_eq!(selected, Some(FAST_GROUNDED_VALIDATED_STRETCH_TARGET));
}
