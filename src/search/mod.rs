//! Compact block lookup, legacy FM readers, and bounded ranked/fuzzy retrieval.
pub mod blocks;
pub(crate) mod candidates;
pub mod fm;
pub mod fuzzy;
pub mod literal;
pub mod planner;
pub mod wavelet;
