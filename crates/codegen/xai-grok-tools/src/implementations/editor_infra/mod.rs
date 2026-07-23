//! Shared editor infrastructure used by the default grok_build tools.
pub mod file_operation_lock;
pub use file_operation_lock::{FileOperationLockGuard, FileOperationLockManager};
