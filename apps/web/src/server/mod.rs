//! Everything that runs only in the server binary: the startup configuration,
//! dashboard authentication, the Restate ingress client, the state the server
//! functions share, and the GitHub webhook edge. Gated once at the crate root
//! so the wasm build never sees it.

pub(crate) mod auth;
pub(crate) mod config;
pub(crate) mod restate;
pub(crate) mod state;
#[cfg(test)]
mod test_support;
pub(crate) mod webhook;
