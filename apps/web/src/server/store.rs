//! The read-model store the server functions query, connected once per
//! process and shared by every call.

use dependaboard_store::{LibSqlPrStore, StoreConfig};
use dioxus::prelude::ServerFnError;

static STORE: tokio::sync::OnceCell<LibSqlPrStore> = tokio::sync::OnceCell::const_new();

pub(crate) async fn store() -> Result<&'static LibSqlPrStore, ServerFnError> {
    STORE
        .get_or_try_init(|| async {
            LibSqlPrStore::connect(&StoreConfig::from_env())
                .await
                .map_err(|error| ServerFnError::new(error.to_string()))
        })
        .await
}
