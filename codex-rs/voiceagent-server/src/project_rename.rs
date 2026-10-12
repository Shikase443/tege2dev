use std::collections::HashSet;
use std::fmt;
use std::path::Path;
use std::sync::Arc;

use anyhow::Context;
use sqlx::Row;
use sqlx::SqliteConnection;
use tokio::sync::Semaphore;

use super::Harness;

#[derive(Debug)]
pub struct RenameRejected(pub &'static str);

impl fmt::Display for RenameRejected {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.0)
    }
}

impl std::error::Error for RenameRejected {}

struct SessionMove {
    old_key: String,
    new_key: String,
    conversation_id: String,
    thread_id: String,
}

impl Harness {
    pub async fn rename_project(
        &self,
        old_name: &str,
        new_name: &str,
        source: &Path,
        destination: &Path,
    ) -> anyhow::Result<()> {
        let _project_change = self.project_changes.lock().await;
        let metadata = tokio::fs::symlink_metadata(source).await?;
        if !metadata.is_dir() || metadata.is_symlink() {
            return Err(RenameRejected("プロジェクトのディレクトリが無効です").into());
        }
        match tokio::fs::symlink_metadata(destination).await {
            Ok(_) => return Err(RenameRejected("同名のプロジェクトが存在します").into()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }

        let old_prefix = format!("project:{old_name}:");
        let new_prefix = format!("project:{new_name}:");
        let rows = sqlx::query(
            "SELECT session_key, conversation_id, thread_id FROM sessions \
             WHERE substr(conversation_id, 1, length(?)) = ?",
        )
        .bind(&old_prefix)
        .bind(&old_prefix)
        .fetch_all(&self.sessions)
        .await?;
        let mut moves = Vec::new();
        for row in rows {
            let old_id: String = row.try_get("conversation_id")?;
            let conversation_id = format!("{new_prefix}{}", &old_id[old_prefix.len()..]);
            moves.push(SessionMove {
                old_key: row.try_get("session_key")?,
                new_key: Self::session_key(&conversation_id),
                conversation_id,
                thread_id: row.try_get("thread_id")?,
            });
        }
        let mut keys = HashSet::from([
            Self::session_key(&format!("{old_prefix}main")),
            Self::session_key(&format!("{new_prefix}main")),
        ]);
        for movement in &moves {
            keys.insert(movement.old_key.clone());
            keys.insert(movement.new_key.clone());
        }
        let mut permits = Vec::new();
        for key in &keys {
            let semaphore = self
                .session_queues
                .semaphores
                .lock()
                .await
                .entry(key.clone())
                .or_insert_with(|| Arc::new(Semaphore::new(1)))
                .clone();
            permits.push(semaphore.try_acquire_owned().map_err(|_| {
                RenameRejected("作業中はプロジェクト名を変更できません")
            })?);
        }

        let mut transaction = self.sessions.begin().await?;
        rename_session_records(&mut transaction, &moves).await?;

        // Unload idle runtimes so the same rollout resumes with the new workspace config.
        for movement in &moves {
            for id in [movement.thread_id.clone()] {
                let thread_id = super::ThreadId::from_string(&id)?;
                if let Ok(thread) = self.manager.get_thread(thread_id).await {
                    thread.flush_rollout().await?;
                    thread.shutdown_and_wait().await?;
                    self.manager.remove_thread(&thread_id).await;
                }
            }
        }
        tokio::fs::rename(source, destination).await?;
        if let Err(error) = transaction.commit().await {
            tokio::fs::rename(destination, source)
                .await
                .context("restore project directory after database commit failure")?;
            return Err(error.into());
        }
        drop(permits);
        // Keep the queue objects: a request already waiting on an old key must still serialize.
        Ok(())
    }
}

async fn rename_session_records(
    connection: &mut SqliteConnection,
    moves: &[SessionMove],
) -> anyhow::Result<()> {
    for movement in moves {
        if sqlx::query("SELECT session_key FROM sessions WHERE session_key = ?")
            .bind(&movement.new_key)
            .fetch_optional(&mut *connection)
            .await?
            .is_some()
        {
            return Err(RenameRejected("変更先の名前に会話が既に紐付いています").into());
        }
        sqlx::query("UPDATE sessions SET session_key = ?, conversation_id = ? WHERE session_key = ?")
            .bind(&movement.new_key)
            .bind(&movement.conversation_id)
            .bind(&movement.old_key)
            .execute(&mut *connection)
            .await?;
    }
    Ok(())
}

#[cfg(test)]
#[path = "project_rename_tests.rs"]
mod tests;
