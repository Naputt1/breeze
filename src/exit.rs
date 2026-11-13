//! Process exit codes used across the CLI.

#![allow(dead_code)] // The full exit-code surface is defined for later phases.

use std::fmt;

/// Success.
pub const SUCCESS: i32 = 0;
/// General failure.
pub const GENERAL: i32 = 1;
/// Usage error, including unimplemented commands in Phase 0.
pub const USAGE: i32 = 2;
/// A patch could not be applied cleanly.
pub const PATCH_CONFLICT: i32 = 3;
/// The repository has not been initialized with `breeze init`.
pub const NOT_INITIALIZED: i32 = 4;
/// The operation was refused because it looked unsafe.
pub const UNSAFE_REFUSED: i32 = 5;
/// The guard blocked the operation.
pub const GUARD_BLOCKED: i32 = 6;

/// An error carrying the process exit code it should produce.
#[derive(Debug)]
pub struct CommandError {
    pub code: i32,
    pub message: String,
}

impl CommandError {
    pub fn new(code: i32, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

impl fmt::Display for CommandError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for CommandError {}
