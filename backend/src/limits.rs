//! Bounded service configuration.

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
            max_input_bytes: env_usize(
                "RYU_ANYDOC_MAX_INPUT_BYTES",
                DEFAULT_MAX_INPUT_BYTES,
                1,
                1024 * 1024 * 1024,
            ),
            max_output_bytes: env_usize(
                "RYU_ANYDOC_MAX_OUTPUT_BYTES",
                DEFAULT_MAX_OUTPUT_BYTES,
                1,
                64 * 1024 * 1024,
            ),
            timeout: Duration::from_secs(env_u64(
                "RYU_ANYDOC_TIMEOUT_SECS",
                DEFAULT_TIMEOUT_SECS,
                1,
                3600,
            )),
            max_workers: env_usize("RYU_ANYDOC_MAX_WORKERS", DEFAULT_MAX_WORKERS, 1, 64),
            max_jobs: env_usize("RYU_ANYDOC_MAX_JOBS", DEFAULT_MAX_JOBS, 1, 4096),
            max_json_body_bytes: env_usize(
                "RYU_ANYDOC_MAX_JSON_BODY_BYTES",
                DEFAULT_MAX_JSON_BODY_BYTES,
                1024,
                256 * 1024 * 1024,
            ),
            max_http_body_bytes: env_usize(
                "RYU_ANYDOC_MAX_HTTP_BODY_BYTES",
                DEFAULT_MAX_HTTP_BODY_BYTES,
                1024,
                512 * 1024 * 1024,
            ),
        }
    }
}

impl Limits {
    #[must_use]
    pub fn timeout_secs(&self) -> u64 {
        self.timeout.as_secs()
    }
}

fn env_usize(name: &str, default: usize, minimum: usize, maximum: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|value| value.trim().parse::<usize>().ok())
        .map(|value| value.clamp(minimum, maximum))
        .unwrap_or(default)
}

fn env_u64(name: &str, default: u64, minimum: u64, maximum: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .map(|value| value.clamp(minimum, maximum))
        .unwrap_or(default)
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
