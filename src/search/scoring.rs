//! Ranking.
//!
//! Scoring is deliberately a separate, testable component: hybrid retrieval is
//! the part of ContextD most likely to be tuned, and burying the formula
//! inside the query code would make that impossible. Weights come from
//! configuration; the [`Scorer`] trait allows an entirely different model
//! later without touching the retrieval code.

use crate::config::SearchConfig;
use crate::core::model::{Category, Status};

/// Signals available about one candidate record.
#[derive(Debug, Clone, PartialEq)]
pub struct Features {
    /// Normalised full-text score, 0.0..=1.0.
    pub fts: f64,
    /// Cosine similarity mapped to 0.0..=1.0.
    pub semantic: f64,
    /// 1..=5.
    pub priority: i64,
    /// Age of the record in days.
    pub age_days: f64,
    /// The record belongs to the project the query is about (rather than
    /// being a global memory or from elsewhere).
    pub same_project: bool,
    /// Lifecycle status; historical records are penalised, never hidden.
    pub status: Status,
    /// Category, used to favour structural knowledge.
    pub category: Option<Category>,
}

impl Default for Features {
    fn default() -> Self {
        Self {
            fts: 0.0,
            semantic: 0.0,
            priority: 3,
            age_days: 0.0,
            same_project: false,
            status: Status::Active,
            category: None,
        }
    }
}

/// Per-component contributions, surfaced by `--explain` and by the MCP tools
/// so a developer can see *why* a memory was retrieved.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize)]
pub struct Breakdown {
    pub fts: f64,
    pub semantic: f64,
    pub priority: f64,
    pub recency: f64,
    pub project: f64,
    /// Multiplier applied for lifecycle status (1.0 for active).
    pub status_multiplier: f64,
    pub total: f64,
}

/// Turns [`Features`] into a comparable score.
pub trait Scorer: Send + Sync {
    fn breakdown(&self, features: &Features) -> Breakdown;

    fn score(&self, features: &Features) -> f64 {
        self.breakdown(features).total
    }
}

/// The default linear scorer.
#[derive(Debug, Clone)]
pub struct WeightedScorer {
    config: SearchConfig,
}

impl WeightedScorer {
    pub fn new(config: SearchConfig) -> Self {
        Self { config }
    }

    /// Exponential decay: `recency_half_life_days` old scores 0.5.
    fn recency(&self, age_days: f64) -> f64 {
        let half_life = self.config.recency_half_life_days.max(1.0);
        0.5_f64.powf(age_days.max(0.0) / half_life)
    }

    /// Current truth outranks historical truth.
    ///
    /// This is the mechanism behind ContextD's second design rule: a
    /// superseded memory keeps its content and stays searchable, but it can no
    /// longer outrank the memory that replaced it merely by being more
    /// numerous or better phrased.
    fn status_multiplier(&self, status: Status) -> f64 {
        match status {
            Status::Active => 1.0,
            Status::Superseded | Status::Deprecated => self.config.superseded_penalty,
            Status::Archived => self.config.archived_penalty,
        }
    }
}

impl Scorer for WeightedScorer {
    fn breakdown(&self, features: &Features) -> Breakdown {
        let priority_norm = ((features.priority.clamp(1, 5) - 1) as f64) / 4.0;
        // Structural knowledge answers "how does this work?" questions, which
        // is what an agent almost always asks; give it a small nudge.
        let structural_bonus =
            if features.category.is_some_and(|c| c.is_structural()) { 0.5 } else { 0.0 };

        let fts = self.config.fts_weight * features.fts;
        let semantic = self.config.semantic_weight * features.semantic;
        let priority = self.config.priority_weight * (priority_norm + structural_bonus * 0.2);
        let recency = self.config.recency_weight * self.recency(features.age_days);
        let project = if features.same_project { self.config.project_weight } else { 0.0 };
        let status_multiplier = self.status_multiplier(features.status);

        let total = (fts + semantic + priority + recency + project) * status_multiplier;
        Breakdown { fts, semantic, priority, recency, project, status_multiplier, total }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scorer() -> WeightedScorer {
        WeightedScorer::new(SearchConfig::default())
    }

    #[test]
    fn recency_halves_at_the_half_life() {
        let s = scorer();
        let half_life = SearchConfig::default().recency_half_life_days;
        assert!((s.recency(0.0) - 1.0).abs() < 1e-9);
        assert!((s.recency(half_life) - 0.5).abs() < 1e-9);
        assert!(s.recency(half_life * 4.0) < 0.1);
    }

    #[test]
    fn current_truth_outranks_superseded_truth() {
        let s = scorer();
        // The superseded memory is a *better* textual match, and newer.
        let superseded = Features {
            fts: 1.0,
            semantic: 1.0,
            status: Status::Superseded,
            same_project: true,
            ..Default::default()
        };
        let current = Features {
            fts: 0.6,
            semantic: 0.6,
            status: Status::Active,
            same_project: true,
            ..Default::default()
        };
        assert!(
            s.score(&current) > s.score(&superseded),
            "active {} should beat superseded {}",
            s.score(&current),
            s.score(&superseded)
        );
    }

    #[test]
    fn archived_is_penalised_more_than_superseded() {
        let s = scorer();
        let base = Features { fts: 1.0, ..Default::default() };
        let superseded = Features { status: Status::Superseded, ..base.clone() };
        let archived = Features { status: Status::Archived, ..base };
        assert!(s.score(&archived) < s.score(&superseded));
    }

    #[test]
    fn project_records_beat_global_ones_at_equal_relevance() {
        let s = scorer();
        let scoped = Features { fts: 0.5, same_project: true, ..Default::default() };
        let global = Features { fts: 0.5, same_project: false, ..Default::default() };
        assert!(s.score(&scoped) > s.score(&global));
    }

    #[test]
    fn priority_and_structure_contribute() {
        let s = scorer();
        let low = Features { fts: 0.5, priority: 1, ..Default::default() };
        let high = Features { fts: 0.5, priority: 5, ..Default::default() };
        assert!(s.score(&high) > s.score(&low));

        let plain = Features { fts: 0.5, category: Some(Category::Task), ..Default::default() };
        let structural =
            Features { fts: 0.5, category: Some(Category::Architecture), ..Default::default() };
        assert!(s.score(&structural) > s.score(&plain));
    }

    #[test]
    fn breakdown_sums_to_total() {
        let s = scorer();
        let f = Features { fts: 0.7, semantic: 0.3, same_project: true, ..Default::default() };
        let b = s.breakdown(&f);
        let sum = (b.fts + b.semantic + b.priority + b.recency + b.project) * b.status_multiplier;
        assert!((sum - b.total).abs() < 1e-12);
    }

    #[test]
    fn weights_are_configurable() {
        let config = SearchConfig { semantic_weight: 0.0, ..SearchConfig::default() };
        let s = WeightedScorer::new(config);
        let only_semantic = Features { semantic: 1.0, ..Default::default() };
        assert!(s.breakdown(&only_semantic).semantic.abs() < 1e-12);
    }
}
