//! Incremental text search over watched directory trees.
//!
//! Exact search uses **Tantivy** (in-RAM) over file bodies and path/filename
//! tokens. Semantic search uses **Candle** (MiniLM, weights embedded at compile
//! time) with cosine similarity over per-file embeddings.
//!
//! Used for agent skills / project guidance discovery and optional manual
//! indexing. Symlinks are never followed. Search requires the target path to
//! be under a previously indexed root (or an auto-discovered skills root).

mod discover;
mod embed_model;
mod engine;
mod index;

pub use discover::{AutoIndexTarget, discover_auto_index_targets};
pub use engine::{
    DropReport, IndexReport, SearchHit, SearchOptions, SearchReport, TextSearchEngine,
};
