//! Supply-chain facts about external packages (supply-chain plan §1 and §2):
//! the native deps.dev client, and the HTTP layer it reads through.
//!
//! [`deps_dev`] is the deps.dev v3 client and the purl decomposition its keys
//! come from. [`http_cache`] is the read-through cache and self-throttle under
//! it. It is HTTP-generic, so the OSV client reuses it under its own namespace.
//! Nothing here writes to the graph store; the store integration is the
//! supply-chain job's business.
pub mod deps_dev;
pub mod http_cache;
