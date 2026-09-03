//! Shared fixtures for the service's unit tests.

use std::sync::{Arc, Mutex};

use dependaboard_core::PrTarget;

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
