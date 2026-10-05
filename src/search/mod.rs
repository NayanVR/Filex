//! Compact block lookup, legacy FM readers, bounded ranked/fuzzy retrieval,
//! and the query side: `key:value` filters and natural-language phrases.
pub mod blocks;
pub(crate) mod candidates;
pub mod filter;
pub mod fm;
pub mod fuzzy;
pub mod literal;
pub mod phrases;
pub mod planner;
pub mod wavelet;
