use super::*;
use chrono::Utc;

impl Harness {
    pub async fn compaction_thread(
        &self, session_key: &str, conversation_id: &str, workspace: Option<&Path>,
    ) -> anyhow::Result<Arc<CodexThread>> {
        if self.stored_session(session_key).await?.is_none() {
            bail!("圧縮する会話セッションがありません。");
        }
        let config = match workspace {
            Some(path) => config_for_workspace(&self.config, path.to_path_buf())?,
            None => self.config.clone(),
        };
        self.thread_for_session(session_key, conversation_id, false, &config).await
    }

    pub async fn compaction_history(&self, session_key: &str) -> anyhow::Result<Vec<serde_json::Value>> {
        let Some(session) = self.stored_session(session_key).await? else { return Ok(Vec::new()); };
        let rows = sqlx::query("SELECT payload FROM compaction_events WHERE thread_id = ? ORDER BY id DESC LIMIT 1000")
            .bind(&session.thread_id).fetch_all(&self.sessions).await?;
        rows.iter().rev().map(|row| {
            let payload: String = row.try_get("payload")?;
            Ok(serde_json::from_str(&payload)?)
        }).collect()
    }

    pub async fn compact_session(
        &self, thread: &CodexThread, session_key: &str, observer: &mpsc::Sender<serde_json::Value>,
    ) -> anyhow::Result<serde_json::Value> {
        let before = thread.token_usage_info().await.map(|info| info.last_token_usage.total_tokens);
        let session = self.stored_session(session_key).await?.context("compaction session is unavailable")?;
        let started = Instant::now();
        let pending = json!({"type": "compaction.started", "timestamp": Utc::now().to_rfc3339(), "beforeTokens": before});
        let record_id = sqlx::query("INSERT INTO compaction_events (thread_id, payload) VALUES (?, ?)")
            .bind(&session.thread_id).bind(pending.to_string()).execute(&self.sessions).await?.last_insert_rowid();
        send_observer(Some(observer), pending).await;
        let result = self.execute_compaction(thread, observer).await;
        let after = thread.token_usage_info().await.map(|info| info.last_token_usage.total_tokens);
        let payload = json!({
            "type": "compaction.completed", "timestamp": Utc::now().to_rfc3339(),
            "status": if result.is_ok() { "completed" } else { "failed" },
            "message": result.err().map(|error| error.to_string()),
            "beforeTokens": before, "afterTokens": after,
            "durationMs": i64::try_from(started.elapsed().as_millis()).unwrap_or(i64::MAX)
        });
        sqlx::query("UPDATE compaction_events SET payload = ? WHERE id = ?")
            .bind(payload.to_string()).bind(record_id).execute(&self.sessions).await?;
        Ok(payload)
    }

    async fn execute_compaction(&self, thread: &CodexThread, observer: &mpsc::Sender<serde_json::Value>) -> anyhow::Result<()> {
        let submission = thread.submit(Op::Compact).await.context("submit compaction")?;
        let mut error = None;
        let mut completed = false;
        loop {
            let event = thread.next_event().await.context("read compaction event")?;
            if event.id != submission { continue; }
            match event.msg {
                EventMsg::ContextCompacted(_) => completed = true,
                EventMsg::ItemCompleted(event) => {
                    if matches!(event.item, TurnItem::ContextCompaction(_)) { completed = true; }
                }
                EventMsg::TokenCount(event) => {
                    if let Some(info) = event.info {
                        send_observer(Some(observer), json!({"type":"context.usage", "usedTokens":info.last_token_usage.total_tokens,"contextWindow":info.model_context_window})).await;
                    }
                }
                EventMsg::Error(event) => error = Some(event.message),
                EventMsg::TurnAborted(_) => bail!("圧縮を停止しました。"),
                EventMsg::TurnComplete(event) => {
                    if let Some(error) = event.error.map(|event| event.message).or(error) { bail!(error); }
                    if !completed { bail!("圧縮が完了しませんでした。"); }
                    return Ok(());
                }
                _ => {}
            }
        }
    }
}
