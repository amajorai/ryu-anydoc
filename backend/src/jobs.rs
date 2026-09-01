//! Bounded submit-and-poll jobs for Core's document.parse contract.

use std::{
    collections::{HashMap, VecDeque},
    path::PathBuf,
    sync::Arc,
};

use serde::Serialize;
use tokio::sync::{Mutex, RwLock, Semaphore};
use uuid::Uuid;

use crate::{
    convert::{convert_bytes, ConversionFailure, ExtractionResult},
    limits::Limits,
};

const MAX_QUEUED_JOBS_PER_WORKER: usize = 4;

#[derive(Debug)]
pub enum InputSource {
    Bytes(Vec<u8>),
    Path(PathBuf),
}

#[derive(Debug)]
pub struct PreparedInput {
    pub source: InputSource,
    pub filename: String,
    pub requested_format: Option<String>,
    pub expected_sha256: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct JobSnapshot {
    pub job_id: String,
    pub status: JobStatus,
    pub filename: String,
    pub created_at: String,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
    pub error: Option<String>,
    pub error_code: Option<String>,
    pub missing_dependencies: Vec<String>,
    pub result: Option<ExtractionResult>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JobSubmitError {
    AtCapacity,
}

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum JobStatus {
    Queued,
    Running,
    Succeeded,
    Failed,
    Cancelled,
}

impl JobStatus {
    #[must_use]
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Succeeded | Self::Failed | Self::Cancelled)
    }
}

struct JobRecord {
    snapshot: JobSnapshot,
    cancelled: bool,
    tenant_id: Option<String>,
}

struct JobTable {
    jobs: HashMap<String, JobRecord>,
    order: VecDeque<String>,
}

#[derive(Clone)]
pub struct JobStore {
    inner: Arc<RwLock<JobTable>>,
    order_lock: Arc<Mutex<()>>,
    workers: Arc<Semaphore>,
    admissions: Arc<Semaphore>,
    limits: Limits,
}

impl JobStore {
    #[must_use]
    pub fn new(limits: Limits) -> Self {
        Self {
            inner: Arc::new(RwLock::new(JobTable {
                jobs: HashMap::new(),
                order: VecDeque::new(),
            })),
            order_lock: Arc::new(Mutex::new(())),
            workers: Arc::new(Semaphore::new(limits.max_workers)),
            admissions: Arc::new(Semaphore::new(
                limits.max_jobs.min(
                    limits
                        .max_workers
                        .saturating_mul(MAX_QUEUED_JOBS_PER_WORKER)
                        .max(1),
                ),
            )),
            limits,
        }
    }

    pub async fn submit(
        &self,
        input: PreparedInput,
        tenant_id: Option<&str>,
    ) -> Result<JobSnapshot, JobSubmitError> {
        // Acquire admission before retaining the input in a spawned task. The
        // worker semaphore alone only bounds conversions that have started; a
        // burst of large queued documents would otherwise keep every body alive
        // while waiting for a worker.
        let admission = self
            .admissions
            .clone()
            .try_acquire_owned()
            .map_err(|_| JobSubmitError::AtCapacity)?;
        let now = chrono_now();
        let snapshot = JobSnapshot {
            job_id: format!("parse_{}", Uuid::new_v4().simple()),
            status: JobStatus::Queued,
            filename: input.filename.clone(),
            created_at: now.clone(),
            started_at: None,
            finished_at: None,
            error: None,
            error_code: None,
            missing_dependencies: Vec::new(),
            result: None,
        };
        let job_id = snapshot.job_id.clone();
        let tenant_id = tenant_id.map(str::to_owned);
        {
            let _order_guard = self.order_lock.lock().await;
            let mut table = self.inner.write().await;
            table.order.push_back(job_id.clone());
            table.jobs.insert(
                job_id.clone(),
                JobRecord {
                    snapshot: snapshot.clone(),
                    cancelled: false,
                    tenant_id,
                },
            );
            evict_locked(&mut table, self.limits.max_jobs);
        }

        let worker = self.clone();
        tokio::spawn(async move {
            worker.run(job_id, input, admission).await;
        });
        Ok(snapshot)
    }

    pub fn try_acquire_worker(&self) -> Option<tokio::sync::OwnedSemaphorePermit> {
        self.workers.clone().try_acquire_owned().ok()
    }

    pub async fn get(&self, job_id: &str, tenant_id: Option<&str>) -> Option<JobSnapshot> {
        self.inner
            .read()
            .await
            .jobs
            .get(job_id)
            .filter(|record| record.tenant_id.as_deref() == tenant_id)
            .map(|record| record.snapshot.clone())
    }

    pub async fn list(&self, limit: usize, tenant_id: Option<&str>) -> Vec<JobSnapshot> {
        let table = self.inner.read().await;
        table
            .order
            .iter()
            .rev()
            .filter_map(|job_id| table.jobs.get(job_id))
            .filter(|record| record.tenant_id.as_deref() == tenant_id)
            .take(limit)
            .map(|record| {
                let mut snapshot = record.snapshot.clone();
                snapshot.result = None;
                snapshot
            })
            .collect()
    }

    pub async fn cancel(&self, job_id: &str, tenant_id: Option<&str>) -> Option<JobSnapshot> {
        let mut table = self.inner.write().await;
        let record = table.jobs.get_mut(job_id)?;
        if record.tenant_id.as_deref() != tenant_id {
            return None;
        }
        if record.snapshot.status.is_terminal() {
            return Some(record.snapshot.clone());
        }
        record.cancelled = true;
        record.snapshot.status = JobStatus::Cancelled;
        record.snapshot.finished_at = Some(chrono_now());
        record.snapshot.error = Some("job cancelled by caller".to_owned());
        record.snapshot.error_code = Some("cancelled".to_owned());
        record.snapshot.result = None;
        Some(record.snapshot.clone())
    }

    async fn run(
        &self,
        job_id: String,
        input: PreparedInput,
        admission: tokio::sync::OwnedSemaphorePermit,
    ) {
        let Ok(worker_permit) = self.workers.clone().acquire_owned().await else {
            self.fail(
                &job_id,
                "worker_unavailable",
                "document worker pool stopped",
            )
            .await;
            return;
        };

        if !self.start(&job_id).await {
            return;
        }

        let limits = self.limits.clone();
        let timeout = limits.timeout;
        let mut conversion = tokio::task::spawn_blocking(move || convert_input(input, &limits));

        match tokio::time::timeout(timeout, &mut conversion).await {
            Ok(Ok(Ok(result))) => self.succeed(&job_id, result).await,
            Ok(Ok(Err(error))) => self.fail_conversion(&job_id, error).await,
            Ok(Err(error)) => {
                self.fail(
                    &job_id,
                    "conversion_failed",
                    &format!("worker failed: {error}"),
                )
                .await;
            }
            Err(_) => {
                self.fail(
                    &job_id,
                    "timeout",
                    &format!(
                        "document extraction exceeded the {}s limit",
                        timeout.as_secs()
                    ),
                )
                .await;
                // Tokio cannot cancel a blocking task once it has started. Keep
                // both capacity permits until that task really exits, otherwise
                // repeated timeouts would defeat the worker and admission caps.
                let _ = tokio::spawn(async move {
                    let _worker_permit = worker_permit;
                    let _admission = admission;
                    let _ = conversion.await;
                });
            }
        }
    }

    async fn start(&self, job_id: &str) -> bool {
        let mut table = self.inner.write().await;
        let Some(record) = table.jobs.get_mut(job_id) else {
            return false;
        };
        if record.cancelled {
            return false;
        }
        record.snapshot.status = JobStatus::Running;
        record.snapshot.started_at = Some(chrono_now());
        true
    }

    async fn succeed(&self, job_id: &str, result: ExtractionResult) {
        let mut table = self.inner.write().await;
        let Some(record) = table.jobs.get_mut(job_id) else {
            return;
        };
        if record.cancelled {
            return;
        }
        record.snapshot.status = JobStatus::Succeeded;
        record.snapshot.finished_at = Some(chrono_now());
        record.snapshot.result = Some(result);
    }

    async fn fail_conversion(&self, job_id: &str, error: ConversionFailure) {
        let mut table = self.inner.write().await;
        let Some(record) = table.jobs.get_mut(job_id) else {
            return;
        };
        if record.cancelled {
            return;
        }
        record.snapshot.status = JobStatus::Failed;
        record.snapshot.finished_at = Some(chrono_now());
        record.snapshot.error = Some(error.message);
        record.snapshot.error_code = Some(error.code);
    }

    async fn fail(&self, job_id: &str, code: &str, message: &str) {
        self.fail_conversion(job_id, ConversionFailure::new(code, message))
            .await;
    }
}

pub fn convert_input(
    input: PreparedInput,
    limits: &Limits,
) -> Result<ExtractionResult, ConversionFailure> {
    let bytes = match input.source {
        InputSource::Bytes(bytes) => bytes,
        InputSource::Path(path) => std::fs::read(path).map_err(|_| {
            ConversionFailure::new("input_rejected", "document file could not be read")
        })?,
    };
    if let Some(expected) = input.expected_sha256.as_deref() {
        let actual = sha256_hex(&bytes);
        if actual != expected {
            return Err(ConversionFailure::new(
                "input_rejected",
                "blob_sha256 does not match the submitted document bytes",
            ));
        }
    }
    convert_bytes(
        &bytes,
        &input.filename,
        input.requested_format.as_deref(),
        limits,
    )
}

fn evict_locked(table: &mut JobTable, max_jobs: usize) {
    let mut scanned = 0;
    while table.jobs.len() > max_jobs && scanned < table.order.len() {
        let Some(job_id) = table.order.pop_front() else {
            break;
        };
        if table
            .jobs
            .get(&job_id)
            .is_some_and(|record| record.snapshot.status.is_terminal())
        {
            table.jobs.remove(&job_id);
        } else {
            table.order.push_back(job_id);
        }
        scanned += 1;
    }
}

fn chrono_now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::{InputSource, JobStatus, JobStore, JobSubmitError, PreparedInput};
    use crate::limits::Limits;

    fn csv_input() -> PreparedInput {
        PreparedInput {
            source: InputSource::Bytes(b"name,value\nRyu,1\n".to_vec()),
            filename: "report.csv".to_owned(),
            requested_format: None,
            expected_sha256: None,
        }
    }

    #[tokio::test]
    async fn submit_and_poll_reaches_a_terminal_success() {
        let store = JobStore::new(Limits::default());
        let initial = store.submit(csv_input(), None).await.expect("job admitted");
        assert_eq!(initial.status, JobStatus::Queued);

        for _ in 0..50 {
            if let Some(snapshot) = store.get(&initial.job_id, None).await {
                if snapshot.status.is_terminal() {
                    assert_eq!(snapshot.status, JobStatus::Succeeded);
                    assert!(snapshot.result.is_some());
                    return;
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("job did not finish");
    }

    #[tokio::test]
    async fn cancelling_a_job_is_terminal_and_has_no_result() {
        let store = JobStore::new(Limits::default());
        let initial = store.submit(csv_input(), None).await.expect("job admitted");
        let cancelled = store
            .cancel(&initial.job_id, None)
            .await
            .expect("job exists");
        assert_eq!(cancelled.status, JobStatus::Cancelled);
        assert!(cancelled.result.is_none());
    }

    #[tokio::test]
    async fn jobs_are_isolated_by_authenticated_tenant() {
        let store = JobStore::new(Limits::default());
        let job = store
            .submit(csv_input(), Some("acme"))
            .await
            .expect("job admitted");

        assert!(store.get(&job.job_id, Some("other")).await.is_none());
        assert!(store.list(10, Some("other")).await.is_empty());
        assert!(store.cancel(&job.job_id, Some("other")).await.is_none());
        assert!(store.get(&job.job_id, Some("acme")).await.is_some());
    }

    #[tokio::test]
    async fn admission_rejects_a_burst_before_retaining_unbounded_inputs() {
        let mut limits = Limits::default();
        limits.max_workers = 1;
        limits.max_jobs = 1;
        let store = JobStore::new(limits);

        store
            .submit(csv_input(), None)
            .await
            .expect("first job admitted");
        assert!(matches!(
            store.submit(csv_input(), None).await,
            Err(JobSubmitError::AtCapacity)
        ));
    }
}
