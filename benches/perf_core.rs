use chrono::Utc;
use cosmos_tui::cache::Cache;
use cosmos_tui::context::WorkContext;
use cosmos_tui::index::{CodebaseIndex, FileIndex, FileSummary, Language};
use cosmos_tui::suggest::SuggestionEngine;
use cosmos_tui::ui::{self, App, ViewMode};
use criterion::{black_box, criterion_group, criterion_main, Criterion};
use ratatui::backend::TestBackend;
use ratatui::Terminal;
use std::collections::HashMap;
use std::path::PathBuf;

fn synthetic_index(file_count: usize) -> CodebaseIndex {
    let root = std::env::temp_dir().join("cosmos-perf-synthetic");
    let mut files = HashMap::with_capacity(file_count);

    for i in 0..file_count {
        let path = PathBuf::from(format!("src/feature_{:03}/file_{:05}.rs", i % 120, i));
        files.insert(
            path.clone(),
            FileIndex {
                path,
                language: Language::Rust,
                loc: 80,
                content_hash: format!("hash-{i}"),
                symbols: Vec::new(),
                dependencies: Vec::new(),
                patterns: Vec::new(),
                complexity: 1.0,
                last_modified: Utc::now(),
                summary: FileSummary::default(),
                layer: None,
                feature: None,
            },
        );
    }

    CodebaseIndex {
        root,
        files,
        index_errors: Vec::new(),
        git_head: Some("deadbeef".to_string()),
    }
}

fn synthetic_app(file_count: usize) -> App {
    let index = synthetic_index(file_count);
    let root = index.root.clone();
    let suggestions = SuggestionEngine::new(index.clone());
    let context = WorkContext {
        branch: "main".to_string(),
        uncommitted_files: Vec::new(),
        staged_files: Vec::new(),
        untracked_files: Vec::new(),
        inferred_focus: None,
        modified_count: 0,
        repo_root: root,
    };
    App::new(index, suggestions, context)
}

fn bench_apply_filter(c: &mut Criterion) {
    let mut app_flat = synthetic_app(10_000);
    app_flat.view_mode = ViewMode::Flat;
    c.bench_function("apply_filter_flat", |b| {
        b.iter(|| {
            app_flat.set_search_query(black_box("file_050"));
            black_box(app_flat.project_selected);
        });
    });

    let mut app_grouped = synthetic_app(10_000);
    app_grouped.view_mode = ViewMode::Grouped;
    c.bench_function("apply_filter_grouped", |b| {
        b.iter(|| {
            app_grouped.set_search_query(black_box("feature_040/file_040"));
            black_box(app_grouped.project_selected);
        });
    });
}

fn bench_render_frame(c: &mut Criterion) {
    let mut app = synthetic_app(4_000);
    app.view_mode = ViewMode::Grouped;

    let backend = TestBackend::new(140, 42);
    let mut terminal = Terminal::new(backend).expect("terminal should initialize");

    c.bench_function("render_frame_main", |b| {
        b.iter(|| {
            terminal
                .draw(|frame| ui::render(frame, &app))
                .expect("draw should succeed");
        });
    });
}

fn bench_index_cache_validation(c: &mut Criterion) {
    let temp = tempfile::tempdir().expect("tempdir");
    std::process::Command::new("git")
        .args(["init"])
        .current_dir(temp.path())
        .output()
        .expect("git init");

    let src_dir = temp.path().join("src");
    std::fs::create_dir_all(&src_dir).expect("create src");
    for i in 0..500 {
        let path = src_dir.join(format!("file_{i:04}.rs"));
        std::fs::write(path, format!("pub fn f{i}() -> usize {{ {i} }}\n"))
            .expect("write synthetic source");
    }

    let index = CodebaseIndex::new(temp.path()).expect("index build");
    let cache = Cache::new(temp.path());
    cache.save_index_cache(&index).expect("save index cache");

    c.bench_function("index_cache_load_validate", |b| {
        b.iter(|| {
            let loaded = cache.load_index_cache(temp.path());
            black_box(loaded.is_some());
        });
    });
}

criterion_group!(
    perf_core,
    bench_apply_filter,
    bench_render_frame,
    bench_index_cache_validation
);
criterion_main!(perf_core);
