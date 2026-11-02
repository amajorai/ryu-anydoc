//! Runtime state shared by the mounted sidecar and standalone API routes.

use std::path::PathBuf;

use crate::{auth::AuthConfig, jobs::JobStore, limits::Limits, paths::roots_from_env};

#[derive(Clone)]
pub struct AnyDocState {
    pub auth: AuthConfig,
    pub limits: Limits,
    pub jobs: JobStore,
    pub roots: Vec<PathBuf>,
}

impl AnyDocState {
    pub fn from_env() -> anyhow::Result<Self> {
        let limits = Limits::default();
        Ok(Self {
            auth: AuthConfig::from_env()?,
            jobs: JobStore::new(limits.clone()),
            limits,
            roots: roots_from_env(),
        })
    }
}
