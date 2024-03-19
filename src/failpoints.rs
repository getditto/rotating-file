//! Definitions of the names of all possible failpoints used in this crate, for testing purposes.

pub const LOCK_CONTEXT: &str = "lock-context";
pub const LOCK_COMPRESSION_HANDLE: &str = "lock-compression-handle";

pub const FLUSH_CURRENT_FILE: &str = "flush-current-file";
pub const SYNC_CURRENT_FILE: &str = "sync-current-file";
