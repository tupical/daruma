//! Stable identity of the event log held by this database.
//!
//! `seq` values and device cursors only mean something inside one log, so a
//! client bound to a server compares this id, never the URL or the token.

use daruma_shared::{CoreError, Result};
use sqlx::SqlitePool;

/// The id written once by migration `0064_server_identity.sql`.
pub async fn server_id(pool: &SqlitePool) -> Result<String> {
    let (id,) =
        sqlx::query_as::<_, (String,)>("SELECT server_id FROM server_identity WHERE id = 1")
            .fetch_one(pool)
            .await
            .map_err(|e| CoreError::storage(e.to_string()))?;
    Ok(id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Db;

    #[tokio::test]
    async fn stays_the_same_within_a_database_and_differs_between_them() {
        let first = Db::memory().await.unwrap();
        first.migrate().await.unwrap();
        let second = Db::memory().await.unwrap();
        second.migrate().await.unwrap();

        let id = server_id(first.pool()).await.unwrap();
        assert_eq!(id.len(), 32);
        assert_eq!(server_id(first.pool()).await.unwrap(), id);
        assert_ne!(server_id(second.pool()).await.unwrap(), id);
    }
}
