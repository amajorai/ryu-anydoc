//! Bounded service configuration.
//!
//! The limits are fixed in code rather than read from the environment, so every
//! node and standalone deployment runs the same bounds.

use std::time::Duration;

pub const DEFAULT_MAX_INPUT_BYTES: usize = 200 * 1024 * 1024;
pub const DEFAULT_MAX_OUTPUT_BYTES: usize = 8 * 1024 * 1024;
pub const DEFAULT_TIMEOUT_SECS: u64 = 600;
pub const DEFAULT_MAX_WORKERS: usize = 2;
pub const DEFAULT_MAX_JOBS: usize = 64;
pub const DEFAULT_MAX_JSON_BODY_BYTES: usize = 64 * 1024 * 1024;
pub const DEFAULT_MAX_HTTP_BODY_BYTES: usize = 256 * 1024 * 1024;

#[derive(Clone, Debug)]
pub struct Limits {
    pub max_input_bytes: usize,
    pub max_output_bytes: usize,
    pub timeout: Duration,
    pub max_workers: usize,
    pub max_jobs: usize,
    pub max_json_body_bytes: usize,
    pub max_http_body_bytes: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_input_bytes: DEFAULT_MAX_INPUT_BYTES,
            max_output_bytes: DEFAULT_MAX_OUTPUT_BYTES,
            timeout: Duration::from_secs(DEFAULT_TIMEOUT_SECS),
            max_workers: DEFAULT_MAX_WORKERS,
            max_jobs: DEFAULT_MAX_JOBS,
            max_json_body_bytes: DEFAULT_MAX_JSON_BODY_BYTES,
            max_http_body_bytes: DEFAULT_MAX_HTTP_BODY_BYTES,
        }
    }
}

impl Limits {
    #[must_use]
    pub fn timeout_secs(&self) -> u64 {
        self.timeout.as_secs()
    }
}

#[cfg(test)]
mod tests {
    use super::{Limits, DEFAULT_MAX_INPUT_BYTES, DEFAULT_TIMEOUT_SECS};

    #[test]
    fn defaults_are_bounded_for_the_provider_contract() {
        let limits = Limits::default();
        assert_eq!(limits.max_input_bytes, DEFAULT_MAX_INPUT_BYTES);
        assert_eq!(limits.timeout_secs(), DEFAULT_TIMEOUT_SECS);
        assert!(limits.max_workers > 0);
        assert!(limits.max_jobs > 0);
    }
}
