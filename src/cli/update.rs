use crate::repository::sync;
use cps_common::errors::CpsiError;

/// Update every configured repository.
pub async fn update() -> Result<(), CpsiError> {
    sync::sync().await
}

/// Update every repository matching an optional name prefix.
pub async fn update_with_prefix(prefix: Option<&str>) -> Result<(), CpsiError> {
    match prefix {
        Some(prefix) => sync::sync_prefix(prefix).await,
        None => update().await,
    }
}
