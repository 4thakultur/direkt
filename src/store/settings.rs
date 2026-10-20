//! Key/value runtime settings. Values may be secrets: never log them.

use super::Store;
use crate::error::AppResult;
use chrono::{DateTime, Utc};

impl Store {
    pub async fn get_setting(&self, key: &str) -> AppResult<Option<String>> {
        Ok(
            sqlx::query_scalar("SELECT value FROM settings WHERE key = ?")
                .bind(key)
                .fetch_optional(&self.pool)
                .await?,
        )
    }

    pub async fn set_setting(&self, key: &str, value: &str, now: DateTime<Utc>) -> AppResult<()> {
        sqlx::query(
            "INSERT INTO settings (key, value, updated_at) VALUES (?1, ?2, ?3)
             ON CONFLICT (key) DO UPDATE SET value = ?2, updated_at = ?3",
        )
        .bind(key)
        .bind(value)
        .bind(now.timestamp_millis())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Set (`Some`) or delete (`None`) several settings at once: all or none.
    pub async fn apply_settings(
        &self,
        changes: &[(&str, Option<String>)],
        now: DateTime<Utc>,
    ) -> AppResult<()> {
        let mut tx = self.pool.begin().await?;
        for (key, value) in changes {
            match value {
                Some(value) => {
                    sqlx::query(
                        "INSERT INTO settings (key, value, updated_at) VALUES (?1, ?2, ?3)
                         ON CONFLICT (key) DO UPDATE SET value = ?2, updated_at = ?3",
                    )
                    .bind(key)
                    .bind(value)
                    .bind(now.timestamp_millis())
                    .execute(&mut *tx)
                    .await?;
                }
                None => {
                    sqlx::query("DELETE FROM settings WHERE key = ?")
                        .bind(key)
                        .execute(&mut *tx)
                        .await?;
                }
            }
        }
        tx.commit().await?;
        Ok(())
    }

    pub async fn delete_setting(&self, key: &str) -> AppResult<()> {
        sqlx::query("DELETE FROM settings WHERE key = ?")
            .bind(key)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn settings_round_trip() {
        let s = Store::open_in_memory().await.unwrap();
        let now = Utc::now();
        assert_eq!(s.get_setting("a").await.unwrap(), None);
        s.set_setting("a", "1", now).await.unwrap();
        s.set_setting("a", "2", now).await.unwrap();
        assert_eq!(s.get_setting("a").await.unwrap().as_deref(), Some("2"));
        s.delete_setting("a").await.unwrap();
        assert_eq!(s.get_setting("a").await.unwrap(), None);
    }

    #[tokio::test]
    async fn settings_apply_together() {
        let s = Store::open_in_memory().await.unwrap();
        let now = Utc::now();
        s.set_setting("gone", "x", now).await.unwrap();
        s.apply_settings(&[("a", Some("1".into())), ("gone", None)], now)
            .await
            .unwrap();
        assert_eq!(s.get_setting("a").await.unwrap().as_deref(), Some("1"));
        assert_eq!(s.get_setting("gone").await.unwrap(), None);
    }
}
