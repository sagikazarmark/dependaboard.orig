//! What every `#[server]` function runs against: the shared Restate client,
//! the read-model connection, and the installation the dashboard is bound to.
//! Built once at startup and attached to each dashboard request as an
//! extension, so a server function extracts it with `Extension<ServerState>`.

use dependaboard_store::LibSqlPrStore;

use crate::server::restate::RestateIngress;

#[derive(Clone)]
pub(crate) struct ServerState {
    pub(crate) ingress: RestateIngress,
    pub(crate) store: LibSqlPrStore,
    /// A per-PR sync or a batch target is refused for a pull request of any
    /// other installation.
    pub(crate) installation_id: u64,
}
