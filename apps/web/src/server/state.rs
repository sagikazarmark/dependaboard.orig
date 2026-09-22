//! What every `#[server]` function runs against: the shared Restate client,
//! the read-model connection, and the installation the dashboard is bound to.
//! Built once at startup and attached to each dashboard request as an
//! extension, so a server function extracts it with `Extension<ServerState>`.

use std::sync::Arc;

use dependaboard_store::ProjectionReader;

use crate::server::restate::RestateIngress;

#[derive(Clone)]
pub(crate) struct ServerState {
    pub(crate) ingress: RestateIngress,
    /// The projection as the dashboard reads it. The reader half only: the
    /// Restate service holds the pen, and a server function that wanted to
    /// write would have nothing to write with.
    pub(crate) store: Arc<dyn ProjectionReader>,
    /// The installation this deployment serves, and the scope of every read
    /// and every action: the table, its total, the sidebar's facets and the
    /// freshness stamp are this installation's alone, as are the batches,
    /// and any request that names a pull request by key — a per-PR sync, a
    /// batch target, or the drawer's reads of a row and its durable state —
    /// is refused for a pull request of any other. It comes from the
    /// deployment's configuration and never from a request, so nothing the
    /// browser sends can widen it.
    pub(crate) installation_id: u64,
}
