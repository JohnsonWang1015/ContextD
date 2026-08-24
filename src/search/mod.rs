//! Retrieval.
//!
//! Two candidate sources — [`fulltext`] and [`semantic`] — are fused and
//! ranked by a [`scoring::Scorer`]. Callers ask [`hybrid::SearchService`] for
//! results and never choose an arm themselves, so turning embeddings on or off
//! changes result quality, not call sites.

pub mod fulltext;
pub mod hybrid;
pub mod indexer;
pub mod scoring;
pub mod semantic;
pub mod vector;

pub use hybrid::{SearchHit, SearchMode, SearchRequest, SearchService};
pub use indexer::{IndexReport, IndexService};
pub use scoring::{Breakdown, Features, Scorer, WeightedScorer};
