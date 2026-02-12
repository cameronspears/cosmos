//! Engine implementation and LLM orchestration for Cosmos.

use anyhow::Result;
use cosmos_core::protocol::{
    ApplyRequest, ApplyResult, ChangeSet, Engine as EngineContract, FixContext as CoreFixContext,
    FixPreview as CoreFixPreview, RepoSnapshot, ReviewFinding as CoreReviewFinding,
    ReviewReport as CoreReviewReport,
};
use cosmos_core::suggest::Suggestion;
use std::future::Future;
use std::pin::Pin;
use uuid::Uuid;

pub mod lab;
pub mod llm;

// Compatibility re-exports for migrated modules that still expect old crate paths.
pub mod cache {
    pub use cosmos_adapters::cache::*;
}

pub mod config {
    pub use cosmos_adapters::config::*;
}

pub mod context {
    pub use cosmos_core::context::*;
}

pub mod git_ops {
    pub use cosmos_adapters::git_ops::*;
}

pub mod grouping {
    pub use cosmos_core::grouping::*;
}

pub mod index {
    pub use cosmos_core::index::*;
}

pub mod suggest {
    pub use cosmos_core::suggest::*;

    pub mod llm {
        pub use crate::llm::*;
    }
}

pub mod util {
    pub use cosmos_adapters::util::*;
}

#[derive(Debug, Default, Clone)]
pub struct CosmosEngine;

impl EngineContract for CosmosEngine {
    fn scan_and_suggest<'a>(
        &'a self,
        repo: &'a RepoSnapshot,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<Suggestion>>> + Send + 'a>> {
        Box::pin(async move {
            let result = llm::run_fast_grounded_with_gate(
                &repo.root,
                &repo.index,
                &repo.context,
                None,
                None,
                llm::SuggestionQualityGateConfig::default(),
            )
            .await?;
            Ok(result.suggestions)
        })
    }

    fn build_preview<'a>(
        &'a self,
        repo: &'a RepoSnapshot,
        suggestion_id: Uuid,
    ) -> Pin<Box<dyn Future<Output = Result<CoreFixPreview>> + Send + 'a>> {
        Box::pin(async move {
            let suggestions = self.scan_and_suggest(repo).await?;
            let suggestion = suggestions
                .into_iter()
                .find(|s| s.id == suggestion_id)
                .ok_or_else(|| anyhow::anyhow!("Suggestion not found"))?;
            let preview = llm::build_fix_preview_from_validated_suggestion(&suggestion);
            Ok(CoreFixPreview {
                summary: preview.problem_summary,
                outcome: preview.outcome,
                files: std::iter::once(suggestion.file.clone())
                    .chain(suggestion.additional_files.clone())
                    .collect(),
                preview_hash: format!("{}", suggestion.id),
            })
        })
    }

    fn apply_with_harness<'a>(
        &'a self,
        _repo: &'a RepoSnapshot,
        _request: ApplyRequest,
    ) -> Pin<Box<dyn Future<Output = Result<ApplyResult>> + Send + 'a>> {
        Box::pin(async move {
            Err(anyhow::anyhow!(
                "apply_with_harness is wired through UI workflow"
            ))
        })
    }

    fn adversarial_review<'a>(
        &'a self,
        change_set: &'a ChangeSet,
        ctx: &'a CoreFixContext,
    ) -> Pin<Box<dyn Future<Output = Result<CoreReviewReport>> + Send + 'a>> {
        Box::pin(async move {
            let files: Vec<_> = change_set
                .files
                .iter()
                .map(|f| (f.path.clone(), String::new(), f.content.clone()))
                .collect();
            let review = llm::verify_changes(
                &files,
                1,
                &[],
                Some(&llm::FixContext {
                    problem_summary: ctx.problem_summary.clone(),
                    outcome: ctx.outcome.clone(),
                    description: ctx.description.clone(),
                    modified_areas: Vec::new(),
                }),
            )
            .await?;

            let findings = review
                .findings
                .into_iter()
                .map(|f| CoreReviewFinding {
                    id: Uuid::new_v4(),
                    file: f.file,
                    line: f.line.map(|line| line as usize),
                    severity: f.severity,
                    title: f.title,
                    detail: f.description,
                })
                .collect();

            Ok(CoreReviewReport {
                findings,
                summary: review.summary,
            })
        })
    }
}
