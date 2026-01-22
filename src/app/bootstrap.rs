use crate::app::messages::BackgroundMessage;
use crate::cache;
use crate::grouping::{Confidence, Layer, LayerOverride};
use crate::suggest;
use crate::suggest::llm::grouping as grouping_llm;
use crate::ui::{App, LoadingState};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc;

/// Initialize cached summaries and kick off AI background tasks.
pub fn init_ai_pipeline(app: &mut App, tx: mpsc::Sender<BackgroundMessage>) {
    // Check for API access (and budgets allow it)
    let mut ai_enabled = suggest::llm::is_available();
    if ai_enabled {
        if let Err(e) = app.config.allow_ai(0.0) {
            ai_enabled = false;
            app.show_toast(&e);
        }
    }

    let cache_manager = cache::Cache::new(&app.repo_path);

    // =========================================================================
    //  SMART SUMMARY CACHING
    // =========================================================================

    // Compute file hashes for change detection
    let file_hashes = cache::compute_file_hashes(&app.index);

    // Optional AI-assisted grouping (safe fallback, low-confidence only)
    let grouping_ai_enabled = true;
    let mut grouping_ai_cache = cache_manager
        .load_grouping_ai_cache()
        .unwrap_or_else(cache::GroupingAiCache::new);
    if grouping_ai_cache.normalize_paths(&app.index.root) {
        let _ = cache_manager.save_grouping_ai_cache(&grouping_ai_cache);
    }
    if grouping_ai_enabled {
        let overrides = cached_grouping_overrides(&app.grouping, &grouping_ai_cache, &file_hashes);
        if !overrides.is_empty() {
            let grouping =
                crate::grouping::generate_grouping_with_overrides(&app.index, &overrides);
            app.apply_grouping_update(grouping);
        }
    }

    // Load cached LLM summaries and apply immediately
    let mut llm_cache = cache_manager
        .load_llm_summaries_cache()
        .unwrap_or_else(cache::LlmSummaryCache::new);
    if llm_cache.normalize_paths(&app.index.root) {
        let _ = cache_manager.save_llm_summaries_cache(&llm_cache);
    }

    // Get all valid cached summaries and load them immediately (instant startup!)
    let cached_summaries = llm_cache.get_all_valid_summaries(&file_hashes);
    let cached_count = cached_summaries.len();
    let total_files = file_hashes.len();

    if !cached_summaries.is_empty() {
        app.update_summaries(cached_summaries);
        eprintln!(
            "  Loaded {} cached summaries ({} files total)",
            cached_count, total_files
        );
    }

    // Discover project context (for better quality summaries)
    let project_context = suggest::llm::discover_project_context(&app.index);
    llm_cache.set_project_context(project_context.clone());

    // Find files that need new/updated summaries
    let mut files_needing_summary = llm_cache.get_files_needing_summary(&file_hashes);

    // Optional privacy/cost control: only summarize changed files (and their immediate blast radius)
    if app.config.summarize_changed_only {
        let changed: std::collections::HashSet<PathBuf> = app
            .context
            .all_changed_files()
            .into_iter()
            .cloned()
            .collect();
        let mut wanted = changed.clone();
        for c in &changed {
            if let Some(file_index) = app.index.files.get(c) {
                for u in &file_index.summary.used_by {
                    wanted.insert(u.clone());
                }
                for d in &file_index.summary.depends_on {
                    wanted.insert(d.clone());
                }
            }
        }
        files_needing_summary.retain(|p| wanted.contains(p));
    }
    let needs_summary_count = files_needing_summary.len();

    // Track if we need to generate summaries (used to control loading state)
    app.needs_summary_generation = needs_summary_count > 0;

    if needs_summary_count > 0 {
        eprintln!("  {} files need summary generation", needs_summary_count);
    } else if cached_count > 0 {
        eprintln!("  All {} summaries loaded from cache", cached_count);
    }

    eprintln!();

    // AI grouping enhancement: low-confidence files only, capped for safety
    if grouping_ai_enabled && ai_enabled {
        let max_files =
            grouping_llm::GROUPING_AI_FILES_PER_REQUEST * grouping_llm::GROUPING_AI_MAX_REQUESTS;
        let candidates = select_grouping_ai_candidates(
            &app.grouping,
            &grouping_ai_cache,
            &file_hashes,
            max_files,
        );

        if !candidates.is_empty() {
            let index_clone = app.index.clone();
            let baseline_grouping = app.grouping.clone();
            let file_hashes_clone = file_hashes.clone();
            let tx_grouping = tx.clone();
            let cache_path = app.repo_path.clone();

            // Process chunks sequentially in a single task to avoid cache races
            tokio::spawn(async move {
                let cache = cache::Cache::new(&cache_path);
                let mut grouping_cache = cache
                    .load_grouping_ai_cache()
                    .unwrap_or_else(cache::GroupingAiCache::new);
                let _ = grouping_cache.normalize_paths(&index_clone.root);

                let mut total_usage = suggest::llm::Usage::default();
                let mut saw_usage = false;

                for chunk in candidates
                    .chunks(grouping_llm::GROUPING_AI_FILES_PER_REQUEST)
                    .take(grouping_llm::GROUPING_AI_MAX_REQUESTS)
                {
                    match grouping_llm::classify_grouping_candidates(&index_clone, chunk).await {
                        Ok((suggestions, usage)) => {
                            for suggestion in suggestions {
                                if let Some(hash) = file_hashes_clone.get(&suggestion.path) {
                                    grouping_cache.set_entry(
                                        suggestion.path.clone(),
                                        cache::GroupingAiEntry {
                                            layer: suggestion.layer,
                                            confidence: suggestion.confidence,
                                            file_hash: hash.clone(),
                                            generated_at: chrono::Utc::now(),
                                        },
                                    );
                                }
                            }

                            if let Some(u) = usage {
                                total_usage.prompt_tokens += u.prompt_tokens;
                                total_usage.completion_tokens += u.completion_tokens;
                                total_usage.total_tokens += u.total_tokens;
                                saw_usage = true;
                            }
                        }
                        Err(e) => {
                            let _ = tx_grouping
                                .send(BackgroundMessage::GroupingEnhanceError(e.to_string()));
                        }
                    }
                }

                let _ = cache.save_grouping_ai_cache(&grouping_cache);

                let overrides = cached_grouping_overrides(
                    &baseline_grouping,
                    &grouping_cache,
                    &file_hashes_clone,
                );
                let usage = if saw_usage { Some(total_usage) } else { None };

                if !overrides.is_empty() {
                    let grouping = crate::grouping::generate_grouping_with_overrides(
                        &index_clone,
                        &overrides,
                    );
                    let _ = tx_grouping.send(BackgroundMessage::GroupingEnhanced {
                        grouping,
                        updated_files: overrides.len(),
                        usage,
                        model: "balanced".to_string(),
                    });
                } else if usage.is_some() {
                    let _ = tx_grouping.send(BackgroundMessage::GroupingEnhanced {
                        grouping: baseline_grouping.clone(),
                        updated_files: 0,
                        usage,
                        model: "balanced".to_string(),
                    });
                }
            });
        }
    }

    // =========================================================================
    //  SEQUENTIAL INIT: Summaries first (builds glossary), then suggestions
    // =========================================================================

    if ai_enabled {
        if !files_needing_summary.is_empty() {
            // Phase 1: Summaries needed - generate them first, suggestions come after
            app.loading = LoadingState::GeneratingSummaries;
            app.pending_suggestions_on_init = true;
            app.summary_progress = Some((0, needs_summary_count));

            let index_clone2 = app.index.clone();
            let context_clone2 = app.context.clone();
            let tx_summaries = tx.clone();
            let cache_path = app.repo_path.clone();
            let file_hashes_clone = file_hashes.clone();

            // Prioritize files for generation
            let (high_priority, medium_priority, low_priority) =
                suggest::llm::prioritize_files_for_summary(
                    &index_clone2,
                    &context_clone2,
                    &files_needing_summary,
                );

            // Show initial cached count
            if cached_count > 0 {
                app.show_toast(&format!(
                    "{}/{} cached · summarizing {}",
                    cached_count, total_files, needs_summary_count
                ));
            }

            // Calculate total file count for progress
            let total_to_process = high_priority.len() + medium_priority.len() + low_priority.len();

            tokio::spawn(async move {
                let cache = cache::Cache::new(&cache_path);

                // Load existing cache to update incrementally
                let mut llm_cache = cache
                    .load_llm_summaries_cache()
                    .unwrap_or_else(cache::LlmSummaryCache::new);

                // Load existing glossary to merge new terms into
                let mut glossary = cache
                    .load_glossary()
                    .unwrap_or_else(cache::DomainGlossary::new);

                let mut all_summaries = HashMap::new();
                let mut total_usage = suggest::llm::Usage::default();
                let mut completed_count = 0usize;

                // Process all priority tiers with parallel batching within each tier
                let priority_tiers = [
                    ("high", high_priority),
                    ("medium", medium_priority),
                    ("low", low_priority),
                ];

                for (_tier_name, files) in priority_tiers {
                    if files.is_empty() {
                        continue;
                    }

                    // Use large batch size (16 files) for faster processing
                    let batch_size = 16;
                    let batches: Vec<_> = files.chunks(batch_size).collect();

                    // Process batches sequentially (llm.rs handles internal parallelism)
                    for batch in batches {
                        if let Ok((summaries, batch_glossary, usage)) =
                            suggest::llm::generate_summaries_for_files(
                                &index_clone2,
                                batch,
                                &project_context,
                            )
                            .await
                        {
                            // Update cache with new summaries
                            for (path, summary) in &summaries {
                                if let Some(hash) = file_hashes_clone.get(path) {
                                    llm_cache.set_summary(
                                        path.clone(),
                                        summary.clone(),
                                        hash.clone(),
                                    );
                                }
                            }
                            // Merge new terms into glossary
                            glossary.merge(&batch_glossary);

                            // Save cache incrementally after each batch
                            let _ = cache.save_llm_summaries_cache(&llm_cache);
                            let _ = cache.save_glossary(&glossary);

                            completed_count += summaries.len();

                            // Send progress update with new summaries
                            let _ = tx_summaries.send(BackgroundMessage::SummaryProgress {
                                completed: completed_count,
                                total: total_to_process,
                                summaries: summaries.clone(),
                            });

                            all_summaries.extend(summaries);
                            if let Some(u) = usage {
                                total_usage.prompt_tokens += u.prompt_tokens;
                                total_usage.completion_tokens += u.completion_tokens;
                                total_usage.total_tokens += u.total_tokens;
                            }
                        }
                    }
                }

                let final_usage = if total_usage.total_tokens > 0 {
                    Some(total_usage)
                } else {
                    None
                };

                // Send final message (summaries already sent via progress, so send empty)
                let _ = tx_summaries.send(BackgroundMessage::SummariesReady {
                    summaries: HashMap::new(),
                    usage: final_usage,
                });
            });
        } else {
            // Phase 2 only: All summaries cached - generate suggestions directly with cached glossary
            app.loading = LoadingState::GeneratingSuggestions;

            let index_clone = app.index.clone();
            let context_clone = app.context.clone();
            let tx_suggestions = tx.clone();
            let cache_clone_path = app.repo_path.clone();
            let repo_memory_context = app.repo_memory.to_prompt_context(12, 900);
            let glossary_clone = app.glossary.clone();

            if !glossary_clone.is_empty() {
                app.show_toast(&format!(
                    "{} glossary terms · generating suggestions",
                    glossary_clone.len()
                ));
            }

            tokio::spawn(async move {
                let mem = if repo_memory_context.trim().is_empty() {
                    None
                } else {
                    Some(repo_memory_context)
                };
                let glossary_ref = if glossary_clone.is_empty() {
                    None
                } else {
                    Some(&glossary_clone)
                };
                match suggest::llm::analyze_codebase(
                    &index_clone,
                    &context_clone,
                    mem,
                    glossary_ref,
                )
                .await
                {
                    Ok((suggestions, usage)) => {
                        // Cache the suggestions
                        let cache = cache::Cache::new(&cache_clone_path);
                        let cache_data = cache::SuggestionsCache::from_suggestions(&suggestions);
                        let _ = cache.save_suggestions_cache(&cache_data);

                        let _ = tx_suggestions.send(BackgroundMessage::SuggestionsReady {
                            suggestions,
                            usage,
                            model: "smart".to_string(),
                        });
                    }
                    Err(e) => {
                        let _ =
                            tx_suggestions.send(BackgroundMessage::SuggestionsError(e.to_string()));
                    }
                }
            });
        }
    }
}

fn cached_grouping_overrides(
    grouping: &crate::grouping::CodebaseGrouping,
    cache: &cache::GroupingAiCache,
    file_hashes: &HashMap<PathBuf, String>,
) -> HashMap<PathBuf, LayerOverride> {
    let mut overrides = HashMap::new();

    for (path, entry) in &cache.entries {
        let Some(hash) = file_hashes.get(path) else {
            continue;
        };
        if !cache.is_file_valid(path, hash) {
            continue;
        }
        if entry.confidence < grouping_llm::GROUPING_AI_MIN_CONFIDENCE {
            continue;
        }
        let Some(assignment) = grouping.file_assignments.get(path) else {
            continue;
        };
        if assignment.confidence != Confidence::Low {
            continue;
        }
        if !matches!(assignment.layer, Layer::Unknown | Layer::Shared) {
            continue;
        }
        if assignment.layer == entry.layer {
            continue;
        }
        overrides.insert(
            path.clone(),
            LayerOverride {
                layer: entry.layer,
                confidence: Confidence::from_score(entry.confidence),
            },
        );
    }

    overrides
}

fn select_grouping_ai_candidates(
    grouping: &crate::grouping::CodebaseGrouping,
    cache: &cache::GroupingAiCache,
    file_hashes: &HashMap<PathBuf, String>,
    max_files: usize,
) -> Vec<PathBuf> {
    let mut candidates: Vec<PathBuf> = grouping
        .file_assignments
        .iter()
        .filter(|(_, assignment)| assignment.confidence == Confidence::Low)
        .filter(|(_, assignment)| matches!(assignment.layer, Layer::Unknown | Layer::Shared))
        .filter(|(path, _)| {
            if let Some(hash) = file_hashes.get(path) {
                !cache.is_file_valid(path, hash)
            } else {
                false
            }
        })
        .map(|(path, _)| path.clone())
        .collect();

    candidates.sort();
    candidates.truncate(max_files);
    candidates
}
