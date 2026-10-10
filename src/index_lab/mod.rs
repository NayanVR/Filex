//! Index v2 experiments. No GPUI dependency and no runtime fallback.
pub mod corpus;
pub mod normalize;
pub use crate::catalog::postings;
pub use crate::search::literal;
pub use crate::search::wavelet;
pub mod workload;
