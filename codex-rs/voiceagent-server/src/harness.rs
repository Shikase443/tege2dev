use std::collections::BTreeMap;
use std::collections::HashMap;
use std::collections::HashSet;
use std::path::Path;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

use anyhow::Context;
use anyhow::bail;
use codex_core_api::AbsolutePathBuf;
use codex_core_api::Arg0DispatchPaths;
use codex_core_api::AuthManager;
use codex_core_api::CodexAppsToolsCache;
use codex_core_api::CodexHomeUserInstructionsProvider;
use codex_core_api::CodexThread;
use codex_core_api::Config;
use codex_core_api::EnvironmentManager;
use codex_core_api::EventMsg;
use codex_core_api::ExecServerRuntimePaths;
use codex_core_api::ExtensionRegistryBuilder;
use codex_core_api::NewThread;
use codex_core_api::SessionSource;
use codex_core_api::SteerSubmission;
use codex_core_api::StartIfIdleSubmission;
use codex_core_api::StartThreadOptions;
use codex_core_api::StateDbHandle;
use codex_core_api::ThreadId;
use codex_core_api::ThreadManager;
use codex_core_api::TurnInputRequest;
use codex_core_api::TurnStartOptions;
use codex_core_api::UserInput;
use codex_core_api::build_models_manager;
use codex_core_api::init_state_db;
use codex_core_api::local_agent_graph_store_from_state_db;
use codex_core_api::resolve_installation_id;
use codex_core_api::thread_store_from_config;
use codex_protocol::items::TurnItem;
use codex_protocol::mcp::ClientMcpExtensions;
use codex_protocol::models::MessagePhase;
use codex_protocol::protocol::AdditionalContextEntry;
use codex_protocol::protocol::AdditionalContextKind;
use codex_protocol::protocol::Op;
use serde_json::json;
use sha2::Digest;
use sha2::Sha256;
use sqlx::Row;
use sqlx::SqlitePool;
use sqlx::sqlite::SqliteConnectOptions;
use sqlx::sqlite::SqlitePoolOptions;
use tokio::sync::Mutex;
use tokio::sync::OwnedSemaphorePermit;
use tokio::sync::Semaphore;
use tokio::sync::mpsc;

use crate::config::build_config;
use crate::config::config_for_workspace;

#[path = "project_rename.rs"]
mod project_rename;
pub use project_rename::RenameRejected;

#[path = "manual_compaction.rs"]
mod manual_compaction;

const RECOVERY_PROMPT: &str = "続けて。直前の依頼やツールを再実行せず、\n既存の会話とツール結果を使って最終回答だけを短く返してください。";
const EMPTY_RESPONSE_FALLBACK: &str =
    "すみません、うまく回答をまとめられませんでした。もう一度お願いします。";
const FIXED_SKILLS_PATH: &str = "/data/skills";
const PERSONAL_SKILLS_RELATIVE_PATH: &str = ".agents/personal-skills";
enum TurnOutput {
    Streamed(String),
    Buffered(String),
    Empty,
}

struct ObservedPhase {
    id: String,
    kind: &'static str,
    title: &'static str,
    started: Instant,
}

#[derive(Clone, Copy, Default)]
struct UsageSnapshot {
    input_tokens: i64,
    output_tokens: i64,
    reasoning_output_tokens: i64,
    total_tokens: i64,
}

impl UsageSnapshot {
    fn delta(self, before: Self) -> Self {
        Self {
            input_tokens: (self.input_tokens - before.input_tokens).max(0),
            output_tokens: (self.output_tokens - before.output_tokens).max(0),
            reasoning_output_tokens: (self.reasoning_output_tokens
                - before.reasoning_output_tokens)
                .max(0),
            total_tokens: (self.total_tokens - before.total_tokens).max(0),
        }
    }
}

pub struct Harness {
    manager: Arc<ThreadManager>,
    auth_manager: Arc<AuthManager>,
    config: Config,
    state_db: Option<StateDbHandle>,
    sessions: SqlitePool,
    session_queues: SessionQueues,
    project_changes: Mutex<()>,
}

#[derive(Debug, Clone)]
pub struct StoredSession {
    pub session_key: String,
    pub conversation_id: String,
    pub thread_id: String,
    pub rollout_path: PathBuf,
    pub created_at: String,
    pub updated_at_epoch: i64,
}

pub struct PreparedTurn {
    pub thread: Arc<CodexThread>,
    pub prompt: String,
    pub turn_instructions: Option<String>,
}

#[derive(Default)]
struct SessionQueues {
    semaphores: Mutex<HashMap<String, Arc<Semaphore>>>,
}

impl SessionQueues {
    async fn acquire(&self, session_key: &str) -> anyhow::Result<OwnedSemaphorePermit> {
        let semaphore = self
            .semaphores
            .lock()
            .await
            .entry(session_key.to_string())
            .or_insert_with(|| Arc::new(Semaphore::new(1)))
            .clone();
        semaphore
            .acquire_owned()
            .await
            .context("session queue closed")
    }

    async fn remove(&self, session_key: &str) {
        self.semaphores.lock().await.remove(session_key);
    }
}

impl Harness {
    pub async fn new(
        arg0_paths: Arg0DispatchPaths,
        model: String,
        workspace: PathBuf,
        database_path: &Path,
    ) -> anyhow::Result<Arc<Self>> {
        let config = build_config(model, workspace, arg0_paths)?;
        let fixed_skills_path = AbsolutePathBuf::try_from(FIXED_SKILLS_PATH)
            .context("resolve fixed skills directory")?;
        let personal_skills_path = config.cwd.join(PERSONAL_SKILLS_RELATIVE_PATH);
        tokio::fs::create_dir_all(personal_skills_path.as_path())
            .await
            .context("create personal skills directory")?;
        let state_db = init_state_db(&config).await;
        let auth_manager =
            AuthManager::shared_from_config(&config, /*enable_codex_api_key_env*/ false).await?;
        let runtime_paths = ExecServerRuntimePaths::from_optional_paths(
            config.codex_self_exe.clone(),
            config.codex_linux_sandbox_exe.clone(),
        )?;
        let environment_manager = Arc::new(
            EnvironmentManager::from_codex_home(
                config.codex_home.clone(),
                Some(runtime_paths),
                config.http_client_factory(),
            )
            .await?,
        );
        let thread_store = thread_store_from_config(&config, state_db.clone());
        let installation_id = resolve_installation_id(&config.codex_home).await?;
        let user_instructions_provider = Arc::new(CodexHomeUserInstructionsProvider::new(
            config.codex_home.clone(),
        ));
        let mut extensions = ExtensionRegistryBuilder::<Config>::new();
        codex_skills_extension::install(&mut extensions, |config: &Config| {
            codex_skills_extension::SkillsExtensionConfig {
                include_instructions: config.include_skill_instructions,
                max_context_tokens: config.skill_max_context_tokens,
                bundled_skills_enabled: config.bundled_skills_enabled(),
                orchestrator_skills_enabled: config.orchestrator_skills_enabled,
                shadow_selection_enabled: false,
            }
        });
        let extensions = extensions.build();
        let manager = Arc::new(ThreadManager::new(
            &config,
            Arc::clone(&auth_manager),
            build_models_manager(&config, Arc::clone(&auth_manager)),
            CodexAppsToolsCache::default(),
            SessionSource::Exec,
            environment_manager,
            Arc::new(extensions),
            user_instructions_provider,
            /*analytics_events_client*/ None,
            Arc::clone(&thread_store),
            local_agent_graph_store_from_state_db(state_db.as_ref()),
            installation_id,
            /*attestation_provider*/ None,
            /*external_time_provider*/ None,
        ));
        manager
            .skills_service()
            .set_extra_roots(vec![fixed_skills_path, personal_skills_path]);

        if let Some(parent) = database_path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let options = SqliteConnectOptions::from_str(&format!(
            "sqlite://{}",
            database_path.to_string_lossy().replace('\\', "/")
        ))?
        .create_if_missing(true);
        let sessions = SqlitePoolOptions::new()
            .max_connections(4)
            .connect_with(options)
            .await?;
        sqlx::query("CREATE TABLE IF NOT EXISTS compaction_events (id INTEGER PRIMARY KEY AUTOINCREMENT, thread_id TEXT NOT NULL, payload TEXT NOT NULL)")
            .execute(&sessions).await?;
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS sessions (\
             session_key TEXT PRIMARY KEY, \
             conversation_id TEXT NOT NULL, \
             thread_id TEXT NOT NULL, \
             rollout_path TEXT NOT NULL, \
             created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP, \
             updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP)",
        )
        .execute(&sessions)
        .await?;
        let harness = Arc::new(Self {
            manager,
            auth_manager,
            config,
            state_db,
            sessions,
            session_queues: SessionQueues::default(),
            project_changes: Mutex::new(()),
        });
        Ok(harness)
    }

    pub fn session_key(conversation_id: &str) -> String {
        let digest = Sha256::digest(format!("default\n{conversation_id}").as_bytes());
        digest.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    pub async fn acquire_session(&self, session_key: &str) -> anyhow::Result<OwnedSemaphorePermit> {
        self.session_queues.acquire(session_key).await
    }

    pub async fn prepare_turn(
        &self,
        session_key: &str,
        conversation_id: &str,
        reset: bool,
        prompt: String,
        turn_instructions: Option<String>,
        workspace: Option<&Path>,
    ) -> anyhow::Result<PreparedTurn> {
        let _project_change = match workspace {
            Some(workspace) => {
                let guard = self.project_changes.lock().await;
                if !tokio::fs::metadata(workspace).await?.is_dir() {
                    bail!("project directory is unavailable");
                }
                Some(guard)
            }
            None => None,
        };
        let thread_config = match workspace {
            Some(workspace) => config_for_workspace(&self.config, workspace.to_path_buf())?,
            None => self.config.clone(),
        };
        let thread = self
            .thread_for_session(session_key, conversation_id, reset, &thread_config)
            .await?;
        Ok(PreparedTurn { thread, prompt, turn_instructions })
    }
    pub async fn list_stored_sessions(&self, limit: i64) -> anyhow::Result<Vec<StoredSession>> {
        let rows = sqlx::query(
            "SELECT session_key, conversation_id, thread_id, rollout_path, created_at, \
             unixepoch(updated_at) AS updated_at_epoch \
             FROM sessions WHERE conversation_id LIKE 'agent:main:api:%' \
             ORDER BY created_at DESC LIMIT ?",
        )
        .bind(limit)
        .fetch_all(&self.sessions)
        .await?;

        rows.into_iter()
            .map(|row| {
                Ok(StoredSession {
                    session_key: row.try_get("session_key")?,
                    conversation_id: row.try_get("conversation_id")?,
                    thread_id: row.try_get("thread_id")?,
                    rollout_path: PathBuf::from(row.try_get::<String, _>("rollout_path")?),
                    created_at: row.try_get("created_at")?,
                    updated_at_epoch: row.try_get("updated_at_epoch")?,
                })
            })
            .collect()
    }

    pub async fn stored_session(&self, session_key: &str) -> anyhow::Result<Option<StoredSession>> {
        let row = sqlx::query(
            "SELECT session_key, conversation_id, thread_id, rollout_path, created_at, \
             unixepoch(updated_at) AS updated_at_epoch \
             FROM sessions WHERE session_key = ?",
        )
        .bind(session_key)
        .fetch_optional(&self.sessions)
        .await?;

        row.map(|row| {
            Ok(StoredSession {
                session_key: row.try_get("session_key")?,
                conversation_id: row.try_get("conversation_id")?,
                thread_id: row.try_get("thread_id")?,
                rollout_path: PathBuf::from(row.try_get::<String, _>("rollout_path")?),
                created_at: row.try_get("created_at")?,
                updated_at_epoch: row.try_get("updated_at_epoch")?,
            })
        })
        .transpose()
    }

    pub async fn project_conversation_id(&self, project: &str) -> anyhow::Result<String> {
        let prefix = format!("project:{project}:");
        let row = sqlx::query(
            "SELECT conversation_id FROM sessions \
             WHERE substr(conversation_id, 1, length(?)) = ? \
             ORDER BY updated_at DESC LIMIT 1",
        )
        .bind(&prefix)
        .bind(&prefix)
        .fetch_optional(&self.sessions)
        .await?;

        Ok(row
            .map(|row| row.try_get::<String, _>("conversation_id"))
            .transpose()?
            .unwrap_or_else(|| format!("{prefix}main")))
    }

    pub async fn forget_project_sessions(&self, project: &str) -> anyhow::Result<()> {
        let conversation_prefix = format!("project:{project}:");
        let rows = sqlx::query(
            "SELECT session_key, thread_id FROM sessions \
             WHERE substr(conversation_id, 1, length(?)) = ?",
        )
        .bind(&conversation_prefix)
        .bind(&conversation_prefix)
        .fetch_all(&self.sessions)
        .await?;

        let mut sessions = rows
            .into_iter()
            .map(|row| {
                Ok((
                    row.try_get::<String, _>("session_key")?,
                    row.try_get::<String, _>("thread_id")?,
                ))
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        let default_session_key = Self::session_key(&format!("{conversation_prefix}main"));
        if !sessions
            .iter()
            .any(|(session_key, _)| session_key == &default_session_key)
        {
            sessions.push((default_session_key, String::new()));
        }

        for (session_key, conversation_thread_id) in sessions {
            let permit = self.session_queues.acquire(&session_key).await?;
            sqlx::query("DELETE FROM compaction_events WHERE thread_id = ?")
                .bind(&conversation_thread_id).execute(&self.sessions).await?;
            sqlx::query("DELETE FROM sessions WHERE session_key = ?")
                .bind(&session_key)
                .execute(&self.sessions)
                .await?;

            for thread_id_text in std::iter::once(conversation_thread_id)
                .filter(|thread_id| !thread_id.is_empty())
            {
                if let Ok(thread_id) = ThreadId::from_string(&thread_id_text) {
                    self.manager.remove_thread(&thread_id).await;
                }
            }
            drop(permit);
            self.session_queues.remove(&session_key).await;
        }
        Ok(())
    }

    pub async fn thread_for_session(
        &self,
        session_key: &str,
        conversation_id: &str,
        reset: bool,
        thread_config: &Config,
    ) -> anyhow::Result<Arc<CodexThread>> {
        if reset {
            sqlx::query("DELETE FROM sessions WHERE session_key = ?")
                .bind(session_key)
                .execute(&self.sessions)
                .await?;
        }

        if !reset
            && let Some(row) =
                sqlx::query("SELECT thread_id, rollout_path FROM sessions WHERE session_key = ?")
                    .bind(session_key)
                    .fetch_optional(&self.sessions)
                    .await?
        {
            let thread_id_text: String = row.try_get("thread_id")?;
            let rollout_path: String = row.try_get("rollout_path")?;
            let thread_id = ThreadId::from_string(&thread_id_text)
                .with_context(|| format!("parse stored thread id {thread_id_text}"))?;
            if let Ok(thread) = self.manager.get_thread(thread_id).await {
                return Ok(thread);
            }

            let NewThread { thread, .. } = self
                .manager
                .resume_thread_from_rollout(
                    thread_config.clone(),
                    PathBuf::from(rollout_path),
                    Arc::clone(&self.auth_manager),
                    None,
                    ClientMcpExtensions::default(),
                )
                .await
                .context("resume Codex thread")?;
            return Ok(thread);
        }

        let NewThread {
            thread_id, thread, ..
        } = self
            .manager
            .start_thread(StartThreadOptions::new(thread_config.clone()))
            .await
            .context("start Codex thread")?;
        let rollout_path = thread
            .rollout_path()
            .context("persistent Codex thread did not return a rollout path")?;
        sqlx::query(
            "INSERT INTO sessions \
             (session_key, conversation_id, thread_id, rollout_path) VALUES (?, ?, ?, ?) \
             ON CONFLICT(session_key) DO UPDATE SET \
             conversation_id = excluded.conversation_id, \
             thread_id = excluded.thread_id, \
             rollout_path = excluded.rollout_path, \
             updated_at = CURRENT_TIMESTAMP",
        )
        .bind(session_key)
        .bind(conversation_id)
        .bind(thread_id.to_string())
        .bind(rollout_path.to_string_lossy().to_string())
        .execute(&self.sessions)
        .await?;
        Ok(thread)
    }

    pub async fn interrupt_turn(&self, thread: &CodexThread) -> anyhow::Result<()> {
        thread
            .submit(Op::Interrupt)
            .await
            .map(|_| ())
            .context("interrupt active turn")
    }

    pub async fn steer_turn(
        &self,
        thread: &CodexThread,
        expected_turn_id: String,
        text: String,
    ) -> anyhow::Result<()> {
        let request = TurnInputRequest::user_input(vec![UserInput::Text {
            text,
            text_elements: Vec::new(),
        }]);
        match thread.steer_turn(request, expected_turn_id).await? {
            SteerSubmission::Steered { .. } => Ok(()),
            SteerSubmission::NotSubmitted { reason } => bail!("turn input was not submitted: {reason:?}"),
        }
    }

    pub async fn run_turn(
        self: Arc<Self>,
        _session_permit: OwnedSemaphorePermit,
        thread: Arc<CodexThread>,
        prompt: String,
        turn_instructions: Option<String>,
        tx: mpsc::Sender<String>,
        response_id: String,
        model: String,
        include_metrics: bool,
        observer: Option<mpsc::Sender<serde_json::Value>>,
    ) {
        let result = self
            .run_turn_inner(
                &thread,
                prompt,
                turn_instructions,
                &tx,
                &response_id,
                &model,
                include_metrics,
                observer.as_ref(),
            )
            .await;
        match result {
            Ok(_) => {},
            Err(error) if error.to_string() == "turn aborted" => {
                send_observer(observer.as_ref(), json!({"type": "turn.interrupted"})).await;
                let _ = tx.send("data: [DONE]\n\n".to_string()).await;
            }
            Err(error) => {
                interrupt_and_wait(&thread).await;
                let payload = json!({
                    "error": {
                        "message": error.to_string(),
                        "type": "server_error",
                        "code": "harness_error"
                    }
                });
                let _ = tx.send(format!("data: {payload}\n\n")).await;
            }
        }
    }

    async fn run_turn_inner(
        &self,
        thread: &CodexThread,
        prompt: String,
        turn_instructions: Option<String>,
        tx: &mpsc::Sender<String>,
        response_id: &str,
        model: &str,
        include_metrics: bool,
        observer: Option<&mpsc::Sender<serde_json::Value>>,
    ) -> anyhow::Result<String> {
        let started = Instant::now();
        send_observer(observer, json!({"type": "turn.started"})).await;
        let usage_before = if include_metrics {
            usage_snapshot(thread).await
        } else {
            UsageSnapshot::default()
        };
        send_sse(
            tx,
            chat_chunk(response_id, model, json!({"role": "assistant"}), None),
            thread,
        )
        .await?;

        let mut skills = Vec::new();
        let output = self
            .execute_turn(
                thread,
                prompt,
                turn_instructions,
                tx,
                response_id,
                model,
                false,
                observer,
                &mut skills,
            )
            .await?;
        let output = if matches!(output, TurnOutput::Empty) {
            self.execute_turn(
                thread,
                RECOVERY_PROMPT.to_string(),
                None,
                tx,
                response_id,
                model,
                true,
                observer,
                &mut skills,
            )
            .await?
        } else {
            output
        };

        let final_answer = match output {
            TurnOutput::Buffered(message) => {
                send_sse(
                    tx,
                    chat_chunk(response_id, model, json!({"content": &message}), None),
                    thread,
                )
                .await?;
                message
            }
            TurnOutput::Empty => {
                send_sse(
                    tx,
                    chat_chunk(
                        response_id,
                        model,
                        json!({"content": EMPTY_RESPONSE_FALLBACK}),
                        None,
                    ),
                    thread,
                )
                .await?;
                EMPTY_RESPONSE_FALLBACK.to_string()
            }
            TurnOutput::Streamed(message) => message,
        };
        let metrics = if include_metrics {
            let duration_ms = i64::try_from(started.elapsed().as_millis()).unwrap_or(i64::MAX);
            let usage = usage_snapshot(thread).await.delta(usage_before);
            Some((duration_ms, usage))
        } else {
            None
        };
        let stop_chunk = chat_stop_chunk(response_id, model, metrics, &skills);
        send_sse(tx, stop_chunk, thread).await?;
        send_sse(tx, "data: [DONE]\n\n".to_string(), thread).await?;
        let duration_ms = i64::try_from(started.elapsed().as_millis()).unwrap_or(i64::MAX);
        let usage = if include_metrics {
            usage_snapshot(thread).await.delta(usage_before)
        } else {
            UsageSnapshot::default()
        };
        send_observer(
            observer,
            json!({
                "type": "turn.completed",
                "durationMs": duration_ms,
                "outputTokens": usage.output_tokens,
                "reasoningTokens": usage.reasoning_output_tokens
            }),
        )
        .await;
        Ok(final_answer)
    }

    async fn execute_turn(
        &self,
        thread: &CodexThread,
        prompt: String,
        turn_instructions: Option<String>,
        tx: &mpsc::Sender<String>,
        response_id: &str,
        model: &str,
        disable_tools: bool,
        observer: Option<&mpsc::Sender<serde_json::Value>>,
        skills: &mut Vec<String>,
    ) -> anyhow::Result<TurnOutput> {
        let mut request = TurnInputRequest::user_input(vec![UserInput::Text {
            text: prompt,
            text_elements: Vec::new(),
        }])
        .on_start(TurnStartOptions {
            disable_tools,
            ..TurnStartOptions::default()
        });
        if let Some(instructions) = turn_instructions.filter(|value| !value.trim().is_empty()) {
            request = request.with_additional_context(BTreeMap::from([(
                "voiceagent_turn_instructions".to_string(),
                AdditionalContextEntry {
                    value: instructions,
                    kind: AdditionalContextKind::Application,
                },
            )]));
        }
        let submission = thread
            .start_turn_if_idle(request)
            .await
            .context("submit user input")?;
        match submission {
            StartIfIdleSubmission::Started { turn_id } => {
                send_observer(observer, json!({"type": "turn.ready", "turnId": turn_id})).await;
            }
            StartIfIdleSubmission::NotSubmitted { reason } => {
                bail!("turn input was not submitted: {reason:?}");
            }
        }

        let mut phase_serial = 0_u64;
        let mut thinking =
            start_observed_phase(observer, &mut phase_serial, "thinking", "思考中").await;
        let mut final_message_items = HashSet::new();
        let mut sent_text = false;
        let mut streamed_text = String::new();
        let mut turn_error = None;
        loop {
            let event = thread.next_event().await.context("read Codex event")?;
            match event.msg {
                EventMsg::TurnStarted(event) => {
                    send_observer(
                        observer,
                        json!({"type": "context.usage", "usedTokens": null, "contextWindow": event.model_context_window}),
                    )
                    .await;
                }
                EventMsg::TokenCount(event) => {
                    if let Some(info) = event.info {
                        send_observer(
                            observer,
                            json!({
                                "type": "context.usage",
                                "usedTokens": info.last_token_usage.total_tokens,
                                "contextWindow": info.model_context_window
                            }),
                        )
                        .await;
                    }
                }
                EventMsg::ItemStarted(event) => {
                    if item_ends_thinking(&event.item) {
                        complete_observed_phase(observer, &mut thinking).await;
                    }
                    if let Some(payload) = observed_item("item.started", &event.item, None) {
                        send_observer(observer, payload).await;
                    }
                    if let TurnItem::AgentMessage(message) = event.item
                        && !matches!(message.phase, Some(MessagePhase::Commentary))
                    {
                        final_message_items.insert(message.id);
                    }
                }
                EventMsg::AgentMessageContentDelta(event)
                    if final_message_items.contains(&event.item_id) =>
                {
                    if !event.delta.is_empty() {
                        sent_text = true;
                        streamed_text.push_str(&event.delta);
                        send_observer(
                            observer,
                            json!({"type": "assistant.delta", "text": event.delta}),
                        )
                        .await;
                        send_sse(
                            tx,
                            chat_chunk(response_id, model, json!({"content": event.delta}), None),
                            thread,
                        )
                        .await?;
                    }
                }
                EventMsg::ItemCompleted(event) => {
                    record_skill_execution(&event.item, skills);
                    let duration_ms = event.started_at_ms.map(|started_at_ms| {
                        event.completed_at_ms.saturating_sub(started_at_ms).max(0)
                    });
                    if let Some(payload) = observed_item("item.completed", &event.item, duration_ms)
                    {
                        send_observer(observer, payload).await;
                    }
                    if item_resumes_thinking(&event.item) {
                        thinking =
                            start_observed_phase(observer, &mut phase_serial, "thinking", "思考中")
                                .await;
                    }
                }
                EventMsg::TurnComplete(event) => {
                    complete_observed_phase(observer, &mut thinking).await;
                    let mut finalizing =
                        start_observed_phase(observer, &mut phase_serial, "control", "応答を確定")
                            .await;
                    let message = event
                        .last_agent_message
                        .filter(|message| !message.trim().is_empty());
                    if sent_text {
                        if let Some(error) = event.error.or(turn_error) {
                            bail!(error.message);
                        }
                        complete_observed_phase(observer, &mut finalizing).await;
                        return Ok(TurnOutput::Streamed(streamed_text));
                    }
                    if let Some(ref message) = message {
                        send_observer(
                            observer,
                            json!({"type": "assistant.delta", "text": message}),
                        )
                        .await;
                    }
                    complete_observed_phase(observer, &mut finalizing).await;
                    return Ok(message
                        .map(TurnOutput::Buffered)
                        .unwrap_or(TurnOutput::Empty));
                }
                EventMsg::Error(event) => turn_error = Some(event),
                EventMsg::TurnAborted(_) => bail!("turn aborted"),
                EventMsg::ExecApprovalRequest(_)
                | EventMsg::ApplyPatchApprovalRequest(_)
                | EventMsg::RequestPermissions(_)
                | EventMsg::RequestUserInput(_)
                | EventMsg::DynamicToolCallRequest(_) => {
                    bail!("turn requested unsupported interactive input")
                }
                _ => {}
            }
        }
    }
}

const SKILL_COMMAND_ROOT: &str = "/data/skills/";

fn record_skill_execution(item: &TurnItem, skills: &mut Vec<String>) {
    let TurnItem::CommandExecution(command) = item else {
        return;
    };
    for skill in executed_skill_names(&command.command) {
        if !skills.contains(&skill) {
            skills.push(skill);
        }
    }
}

fn executed_skill_names(command: &[String]) -> Vec<String> {
    let command = command.join(" ");
    let mut names = Vec::new();
    let mut remainder = command.as_str();
    while let Some((_, suffix)) = remainder.split_once(SKILL_COMMAND_ROOT) {
        let path = suffix
            .split(|character: char| {
                character.is_whitespace()
                    || matches!(character, '\'' | '"' | ';' | '|' | ')' | '}' | ']')
            })
            .next()
            .unwrap_or_default();
        let mut components = path.splitn(2, '/');
        let name = components.next().unwrap_or_default();
        let skill_path = components.next().unwrap_or_default();
        if !name.is_empty()
            && !skill_path.is_empty()
            && skill_path != "SKILL.md"
            && name.chars().all(|character| {
                character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.')
            })
            && !names.iter().any(|existing| existing == name)
        {
            names.push(name.to_string());
        }
        remainder = suffix;
    }
    names
}

fn observed_item(
    event_type: &str,
    item: &TurnItem,
    duration_ms: Option<i64>,
) -> Option<serde_json::Value> {
    let mut payload = match item {
        TurnItem::CommandExecution(command) => Some(json!({
            "type": event_type,
            "kind": "command",
            "title": command.command.join(" "),
            "status": command.status,
            "output": command.aggregated_output.as_deref().unwrap_or_default()
        })),
        TurnItem::DynamicToolCall(call) => Some(json!({
            "type": event_type,
            "kind": "tool",
            "title": call.tool,
            "status": call.status,
            "output": call.error.as_deref().unwrap_or_default()
        })),
        TurnItem::McpToolCall(call) => Some(json!({
            "type": event_type,
            "kind": "tool",
            "title": format!("{} / {}", call.server, call.tool),
            "status": call.status
        })),
        TurnItem::FileChange(change) => Some(json!({
            "type": event_type,
            "kind": "file",
            "title": format!("{} file change(s)", change.changes.len()),
            "status": change.status
        })),
        TurnItem::ContextCompaction(_) => Some(json!({
            "type": event_type,
            "kind": "context",
            "title": "会話コンテキストを整理"
        })),
        _ => None,
    }?;
    let object = payload.as_object_mut()?;
    object.insert("id".to_string(), json!(item.id()));
    if let Some(duration_ms) = duration_ms {
        object.insert("durationMs".to_string(), json!(duration_ms));
    }
    Some(payload)
}

fn item_ends_thinking(item: &TurnItem) -> bool {
    matches!(
        item,
        TurnItem::AgentMessage(_)
            | TurnItem::CommandExecution(_)
            | TurnItem::DynamicToolCall(_)
            | TurnItem::McpToolCall(_)
            | TurnItem::FileChange(_)
            | TurnItem::ContextCompaction(_)
    )
}

fn item_resumes_thinking(item: &TurnItem) -> bool {
    matches!(
        item,
        TurnItem::CommandExecution(_)
            | TurnItem::DynamicToolCall(_)
            | TurnItem::McpToolCall(_)
            | TurnItem::FileChange(_)
            | TurnItem::ContextCompaction(_)
    )
}

async fn start_observed_phase(
    observer: Option<&mpsc::Sender<serde_json::Value>>,
    serial: &mut u64,
    kind: &'static str,
    title: &'static str,
) -> Option<ObservedPhase> {
    if observer.is_none() {
        return None;
    }
    *serial = serial.saturating_add(1);
    let phase = ObservedPhase {
        id: format!("harness-{kind}-{}", *serial),
        kind,
        title,
        started: Instant::now(),
    };
    send_observer(
        observer,
        json!({
            "type": "item.started",
            "id": phase.id,
            "kind": phase.kind,
            "title": phase.title
        }),
    )
    .await;
    Some(phase)
}

async fn complete_observed_phase(
    observer: Option<&mpsc::Sender<serde_json::Value>>,
    phase: &mut Option<ObservedPhase>,
) {
    let Some(phase) = phase.take() else {
        return;
    };
    let duration_ms = i64::try_from(phase.started.elapsed().as_millis()).unwrap_or(i64::MAX);
    send_observer(
        observer,
        json!({
            "type": "item.completed",
            "id": phase.id,
            "kind": phase.kind,
            "title": phase.title,
            "status": "completed",
            "durationMs": duration_ms
        }),
    )
    .await;
}

async fn send_observer(
    observer: Option<&mpsc::Sender<serde_json::Value>>,
    payload: serde_json::Value,
) {
    if let Some(observer) = observer {
        let _ = observer.send(payload).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn session_queue_serializes_the_same_key() {
        let queues = Arc::new(SessionQueues::default());
        let first = queues.acquire("same").await.expect("first permit");
        let waiting_queues = Arc::clone(&queues);
        let second =
            tokio::spawn(
                async move { waiting_queues.acquire("same").await.expect("second permit") },
            );

        tokio::task::yield_now().await;
        assert!(!second.is_finished());
        drop(first);
        tokio::time::timeout(Duration::from_secs(1), second)
            .await
            .expect("second permit timeout")
            .expect("second permit task");
    }

    #[tokio::test]
    async fn session_queue_allows_different_keys() {
        let queues = SessionQueues::default();
        let _first = queues.acquire("first").await.expect("first permit");
        tokio::time::timeout(Duration::from_secs(1), queues.acquire("second"))
            .await
            .expect("different session timeout")
            .expect("different session permit");
    }

    #[test]
    fn usage_snapshot_delta_never_goes_negative() {
        let before = UsageSnapshot {
            input_tokens: 100,
            output_tokens: 20,
            reasoning_output_tokens: 8,
            total_tokens: 120,
        };
        let after = UsageSnapshot {
            input_tokens: 140,
            output_tokens: 32,
            reasoning_output_tokens: 6,
            total_tokens: 172,
        };

        let delta = after.delta(before);
        assert_eq!(delta.input_tokens, 40);
        assert_eq!(delta.output_tokens, 12);
        assert_eq!(delta.reasoning_output_tokens, 0);
        assert_eq!(delta.total_tokens, 52);
    }

    #[test]
    fn extracts_executed_skill_names_without_skill_document_reads() {
        assert_eq!(
            executed_skill_names(&[
                "/bin/bash".to_string(),
                "-lc".to_string(),
                "python3 /data/skills/sesame-lock/scripts/sesame_lock.py lock"
                    .to_string(),
            ]),
            vec!["sesame-lock".to_string()]
        );
        assert!(
            executed_skill_names(&[
                "/bin/bash".to_string(),
                "-lc".to_string(),
                "cat /data/skills/sesame-lock/SKILL.md".to_string(),
            ])
            .is_empty()
        );
    }

    #[test]
    fn deduplicates_skill_names_within_one_command() {
        assert_eq!(
            executed_skill_names(&[format!(
                "python3 {root}camera-eye/scripts/capture.py && python3 {root}camera-eye/scripts/capture.py",
                root = SKILL_COMMAND_ROOT
            )]),
            vec!["camera-eye".to_string()]
        );
    }
}

async fn usage_snapshot(thread: &CodexThread) -> UsageSnapshot {
    thread
        .token_usage_info()
        .await
        .map(|info| UsageSnapshot {
            input_tokens: info.total_token_usage.input_tokens,
            output_tokens: info.total_token_usage.output_tokens,
            reasoning_output_tokens: info.total_token_usage.reasoning_output_tokens,
            total_tokens: info.total_token_usage.total_tokens,
        })
        .unwrap_or_default()
}

async fn interrupt_and_wait(thread: &CodexThread) {
    if thread.submit(Op::Interrupt).await.is_err() {
        return;
    }

    let _ = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let Ok(event) = thread.next_event().await else {
                return;
            };
            if matches!(
                event.msg,
                EventMsg::TurnAborted(_) | EventMsg::TurnComplete(_)
            ) {
                return;
            }
        }
    })
    .await;
}

fn chat_chunk(
    response_id: &str,
    model: &str,
    delta: serde_json::Value,
    finish_reason: Option<&str>,
) -> String {
    let payload = json!({
        "id": response_id,
        "object": "chat.completion.chunk",
        "created": std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs(),
        "model": model,
        "choices": [{
            "index": 0,
            "delta": delta,
            "finish_reason": finish_reason
        }]
    });
    format!("data: {payload}\n\n")
}

fn chat_stop_chunk(
    response_id: &str,
    model: &str,
    metrics: Option<(i64, UsageSnapshot)>,
    skills: &[String],
) -> String {
    let mut payload = json!({
        "id": response_id,
        "object": "chat.completion.chunk",
        "created": std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs(),
        "model": model,
        "choices": [{
            "index": 0,
            "delta": {},
            "finish_reason": "stop"
        }],
        "codexagent": {
            "skills": skills
        }
    });
    if let Some((duration_ms, usage)) = metrics {
        let effective_tps = if duration_ms > 0 {
            Some(usage.output_tokens as f64 * 1000.0 / duration_ms as f64)
        } else {
            None
        };
        payload["usage"] = json!({
            "prompt_tokens": usage.input_tokens,
            "completion_tokens": usage.output_tokens,
            "total_tokens": usage.total_tokens,
            "completion_tokens_details": {
                "reasoning_tokens": usage.reasoning_output_tokens
            }
        });
        payload["codexagent_metrics"] = json!({
            "duration_ms": duration_ms,
            "output_tokens": usage.output_tokens,
            "reasoning_tokens": usage.reasoning_output_tokens,
            "effective_tps": effective_tps
        });
    }
    format!("data: {payload}\n\n")
}

async fn send_sse(
    tx: &mpsc::Sender<String>,
    payload: String,
    thread: &CodexThread,
) -> anyhow::Result<()> {
    if tx.send(payload).await.is_err() {
        let _ = thread.submit(Op::Interrupt).await;
        bail!("client disconnected");
    }
    Ok(())
}
