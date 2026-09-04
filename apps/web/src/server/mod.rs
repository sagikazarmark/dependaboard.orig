//! Everything that runs only in the server binary: dashboard authentication,
//! the Restate ingress client, the read-model store, the installation the
//! dashboard is bound to, and the GitHub webhook edge. Gated once at the crate
//! root so the wasm build never sees it.

pub(crate) mod auth;
pub(crate) mod installation;
pub(crate) mod restate;
pub(crate) mod store;
#[cfg(test)]
mod test_support;
pub(crate) mod webhook;
