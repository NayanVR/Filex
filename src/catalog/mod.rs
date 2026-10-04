//! Immutable catalog storage, native names, and durable generation metadata.
//! `segment` owns the production column layout; `storage` owns mapped bytes.
pub(crate) mod columns;
pub mod manifest;
pub mod normalize;
pub mod path_codec;
pub(crate) mod pool;
pub mod postings;
pub mod segment;
pub mod storage;
pub mod wal;
