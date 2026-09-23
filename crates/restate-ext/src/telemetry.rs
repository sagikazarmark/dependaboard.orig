//! The subscriber a Restate service wants: one that does not log a replay twice.

use tracing_subscriber::{EnvFilter, Layer, layer::SubscriberExt, util::SubscriberInitExt};

/// Installs a formatting subscriber that drops the lines a replay re-emits, reading its
/// directives from `RUST_LOG` and falling back to `default_directives`.
///
/// Restate re-runs handler code while replaying a journal, so without
/// [`ReplayAwareFilter`](restate_sdk::filter::ReplayAwareFilter) every line a handler
/// logged before it suspended would be logged again on resume — the completion line
/// [`traced`](crate::handler::traced) writes included.
///
/// Panics if a global subscriber is already set, as `tracing`'s own `init` does; call it
/// once, at startup.
pub fn init_replay_aware_tracing(default_directives: &str) {
    let default_directives = default_directives.to_owned();
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::fmt::layer()
                .with_filter(
                    EnvFilter::try_from_default_env()
                        .unwrap_or_else(|_| EnvFilter::new(default_directives)),
                )
                .with_filter(restate_sdk::filter::ReplayAwareFilter),
        )
        .init();
}
