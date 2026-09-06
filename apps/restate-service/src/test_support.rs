//! Shared fixtures for the service's unit tests.

mod memory_store;

use std::sync::{Arc, Mutex};

use dependaboard_core::{
    CheckStatus, Mergeable, PrKey, PrRecord, PrTarget, RepoRecord, Retirement, UpdateType,
};
use restate_sdk::prelude::*;

pub(crate) use memory_store::MemoryPrStore;

use crate::{pull_request::ClosedRequest, retirement::RetirementEffects};

/// Stands in for Restate and the store's retirement outbox during a drain and records
/// what the drain asked of them: `pending` is what the outbox holds; `closed` and
/// `acknowledged` are what the drain sent and forgot.
#[derive(Default)]
pub(crate) struct RecordedRetirements {
    pub(crate) pending: Vec<Retirement>,
    pub(crate) read_failure: Option<HandlerError>,
    pub(crate) closed: Vec<(PrKey, ClosedRequest)>,
    pub(crate) acknowledged: Vec<u64>,
}

impl RecordedRetirements {
    /// An outbox holding `keys`, queued in order under `synced_before` as their fence.
    pub(crate) fn queued(keys: &[PrKey], synced_before: Option<u64>) -> Self {
        Self {
            pending: keys
                .iter()
                .enumerate()
                .map(|(index, key)| Retirement {
                    id: index as u64 + 1,
                    key: key.clone(),
                    synced_before,
                })
                .collect(),
            ..Default::default()
        }
    }

    /// The keys the drain closed, in order.
    pub(crate) fn closed_keys(&self) -> Vec<PrKey> {
        self.closed.iter().map(|(key, _)| key.clone()).collect()
    }
}

impl RetirementEffects for RecordedRetirements {
    async fn pending_retirements(&mut self) -> HandlerResult<Vec<Retirement>> {
        match self.read_failure.take() {
            Some(error) => Err(error),
            None => Ok(self.pending.clone()),
        }
    }

    fn close_pull_request(&mut self, key: &PrKey, request: ClosedRequest) {
        self.closed.push((key.clone(), request));
    }

    async fn acknowledge_retirements(&mut self, through: u64) -> HandlerResult<()> {
        self.acknowledged.push(through);
        self.pending.retain(|retirement| retirement.id > through);
        Ok(())
    }
}

/// A merge/command target for pull request 9 in `acme/api`, repository 7.
pub(crate) fn target() -> PrTarget {
    PrTarget {
        repository_id: 7,
        owner: "acme".to_owned(),
        repo: "api".to_owned(),
        number: 9,
        expected_sha: "abc123".to_owned(),
        title: "Bump serde".to_owned(),
        html_url: "https://github.com/acme/api/pull/9".to_owned(),
    }
}

/// The repository [`target`] lives in: `acme/api`, repository 7 of installation 1, with
/// no merge-method override.
pub(crate) fn repository() -> RepoRecord {
    RepoRecord {
        repository_id: 7,
        installation_id: 1,
        owner: "acme".to_owned(),
        repo: "api".to_owned(),
        merge_method: None,
        synced_at: 0,
    }
}

/// The canonical snapshot [`target`] was taken from: pull request 9 in `acme/api` at `abc123`.
pub(crate) fn snapshot() -> PrRecord {
    PrRecord {
        id: "7#9".to_owned(),
        repository_id: 7,
        installation_id: 1,
        owner: "acme".to_owned(),
        repo: "api".to_owned(),
        number: 9,
        title: "Bump serde".to_owned(),
        html_url: "https://github.com/acme/api/pull/9".to_owned(),
        dependency: Some("serde".to_owned()),
        from_version: None,
        to_version: None,
        dependencies: Vec::new(),
        update_type: UpdateType::Unknown,
        head_sha: "abc123".to_owned(),
        check_status: CheckStatus::None,
        mergeable: Mergeable::Unknown,
        labels: Vec::new(),
        created_at: 0,
        updated_at: 0,
        synced_at: 0,
    }
}

/// An `io::Write` the test subscriber can hand out repeatedly.
#[derive(Clone, Default)]
pub(crate) struct LogSink(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for LogSink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl LogSink {
    pub(crate) fn subscriber(&self) -> impl tracing::Subscriber + Send + Sync {
        let sink = self.clone();
        tracing_subscriber::fmt()
            .with_writer(move || sink.clone())
            .with_ansi(false)
            .with_max_level(tracing::Level::TRACE)
            .finish()
    }

    pub(crate) fn contents(&self) -> String {
        String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
    }
}

/// Everything the service logs while `run` executes, as the operator would see it.
pub(crate) fn captured_logs(run: impl FnOnce()) -> String {
    let sink = LogSink::default();
    tracing::subscriber::with_default(sink.subscriber(), run);
    sink.contents()
}
