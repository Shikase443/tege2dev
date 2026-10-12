mod config;
mod harness;
mod history;
mod llm_health;

use std::convert::Infallible;
use std::net::SocketAddr;
use std::path::Component;
use std::path::Path as FsPath;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use anyhow::Context;
use axum::Json;
use axum::Router;
use axum::body::Body;
use axum::extract::Path;
use axum::extract::State;
use axum::extract::WebSocketUpgrade;
use axum::extract::ws::Message;
use axum::extract::ws::WebSocket;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::response::Response;
use axum::routing::delete;
use axum::routing::get;
use axum::routing::post;
use bytes::Bytes;
use codex_core_api::Arg0DispatchPaths;
use codex_core_api::arg0_dispatch_or_else;
use futures::StreamExt;
use serde::Deserialize;
use serde_json::Value;
use serde_json::json;
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tracing::info;
use tracing_subscriber::EnvFilter;
use uuid::Uuid;

use crate::harness::Harness;
use crate::history::read_history;
use crate::llm_health::LlmHealth;

#[derive(Clone)]
struct AppState {
    harness: Arc<Harness>,
    api_key: Arc<String>,
    model: Arc<String>,
    webapps_root: Arc<PathBuf>,
    workspace_root: Arc<PathBuf>,
    projects_enabled: bool,
    llm_health: LlmHealth,
}

#[derive(Debug, Deserialize)]
struct ChatRequest {
    #[allow(dead_code)]
    model: Option<String>,
    messages: Vec<ChatMessage>,
    stream: Option<bool>,
    user: Option<String>,
    #[serde(rename = "_meta")]
    meta: Option<RequestMeta>,
}

#[derive(Debug, Deserialize)]
struct ChatMessage {
    role: String,
    content: Value,
}

#[derive(Debug, PartialEq, Eq)]
struct ChatTurnInput {
    prompt: String,
    instructions: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RequestMeta {
    session_key: Option<String>,
    #[serde(default)]
    reset_session: bool,
    #[serde(default)]
    include_metrics: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AgentWsRequest {
    session_id: String,
    #[serde(default)]
    text: String,
    #[serde(default)]
    compact: bool,
    #[serde(default)]
    reset: bool,
    project: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum AgentWsControl {
    #[serde(rename = "turn.interrupt")]
    TurnInterrupt,
    #[serde(rename = "turn.steer")]
    TurnSteer { text: String },
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum AgentWsMessage {
    Turn(AgentWsRequest),
    Control(AgentWsControl),
}

fn main() -> anyhow::Result<()> {
    arg0_dispatch_or_else(run_main)
}

async fn run_main(arg0_paths: Arg0DispatchPaths) -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_target(false)
        .init();

    let model = required_env("CODEX_AGENT_MODEL")?;
    let api_key = required_env("LOCAL_AGENT_API_KEY")?;
    let workspace = PathBuf::from(
        std::env::var("CODEX_AGENT_WORKSPACE").unwrap_or_else(|_| "/workspace".to_string()),
    );
    let data_dir = PathBuf::from(
        std::env::var("CODEX_AGENT_DATA_DIR").unwrap_or_else(|_| "/data".to_string()),
    );
    let webapps_root = workspace.join("webapps");
    tokio::fs::create_dir_all(&webapps_root).await?;
    let harness = Harness::new(
        arg0_paths,
        model.clone(),
        workspace.clone(),
        &data_dir.join("sessions.sqlite3"),
    )
    .await?;
    let state = AppState {
        harness,
        api_key: Arc::new(api_key),
        model: Arc::new(model),
        webapps_root: Arc::new(webapps_root),
        workspace_root: Arc::new(workspace),
        projects_enabled: env_flag("CODEX_AGENT_PROJECTS_ENABLED"),
        llm_health: LlmHealth::start(&std::env::var("CODEX_OSS_BASE_URL").unwrap_or_default()),
    };

    let app = Router::new()
        .route("/health", get(health))
        .route("/v1/agent/llm-health", get(llm_health))
        .route("/v1/models", get(models))
        .route("/v1/projects/{project}", delete(delete_project))
        .route("/v1/projects/{project}/rename", post(rename_project))
        .route("/v1/projects/{project}/history", get(project_history))
        .route("/v1/projects/{project}/publish", post(publish_project))
        .route("/v1/chat/completions", post(chat_completions))
        .route("/v1/agent/ws", get(agent_websocket))
        .route("/v1/history/sessions", get(history_sessions))
        .route("/v1/history/sessions/{session_key}", get(session_history))
        .route("/apps/{*path}", get(web_app))
        .with_state(state);
    let port = std::env::var("PORT")
        .ok()
        .and_then(|value| value.parse::<u16>().ok())
        .unwrap_or(8091);
    let address = SocketAddr::from(([0, 0, 0, 0], port));
    let listener = TcpListener::bind(address).await?;
    info!(%address, "codex voiceagent server listening");
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    Ok(())
}

async fn health() -> impl IntoResponse {
    Json(json!({"status": "ok"}))
}

async fn llm_health(State(state): State<AppState>) -> impl IntoResponse {
    Json(state.llm_health.response().await)
}

#[derive(Deserialize)]
struct RenameProjectRequest {
    name: String,
}

async fn rename_project(
    Path(project): Path<String>,
    State(state): State<AppState>,
    Json(request): Json<RenameProjectRequest>,
) -> Response {
    if !state.projects_enabled {
        return api_error(StatusCode::NOT_FOUND, "projects_disabled", "Project management is disabled");
    }
    let name = request.name.trim();
    if !safe_project_name(&project) || !safe_project_name(name) || project == name {
        return api_error(StatusCode::BAD_REQUEST, "invalid_project_name", "別の有効なプロジェクト名を指定してください");
    }
    let root = state.workspace_root.as_ref();
    match state.harness.rename_project(&project, name, &root.join(&project), &root.join(name)).await {
        Ok(()) => Json(json!({"name": name, "previousName": project})).into_response(),
        Err(error) => {
            let status = if error.downcast_ref::<harness::RenameRejected>().is_some() {
                StatusCode::CONFLICT
            } else if error.downcast_ref::<std::io::Error>().is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound) {
                StatusCode::NOT_FOUND
            } else {
                StatusCode::INTERNAL_SERVER_ERROR
            };
            api_error(status, "project_rename_failed", &error.to_string())
        }
    }
}

async fn delete_project(Path(project): Path<String>, State(state): State<AppState>) -> Response {
    let (name, project_path) = match resolve_project(&state, Some(&project)).await {
        Ok(Some(project)) => project,
        Ok(None) => {
            return api_error(
                StatusCode::NOT_FOUND,
                "projects_disabled",
                "Project management is disabled",
            );
        }
        Err(error) => {
            return api_error(
                StatusCode::NOT_FOUND,
                "project_not_found",
                &error.to_string(),
            );
        }
    };

    if let Err(error) = state.harness.forget_project_sessions(&name).await {
        return api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "project_session_delete_failed",
            &error.to_string(),
        );
    }
    match tokio::fs::remove_dir_all(project_path).await {
        Ok(()) => Json(json!({"deleted": true, "name": name})).into_response(),
        Err(error) => api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "project_delete_failed",
            &error.to_string(),
        ),
    }
}

async fn publish_project(Path(project): Path<String>, State(state): State<AppState>) -> Response {
    let project_path = match resolve_project(&state, Some(&project)).await {
        Ok(Some((_, path))) => path,
        Ok(None) => {
            return api_error(
                StatusCode::NOT_FOUND,
                "projects_disabled",
                "Project management is disabled",
            );
        }
        Err(error) => {
            return api_error(
                StatusCode::NOT_FOUND,
                "project_not_found",
                &error.to_string(),
            );
        }
    };
    if !tokio::fs::metadata(project_path.join("index.html"))
        .await
        .is_ok_and(|metadata| metadata.is_file())
    {
        return api_error(
            StatusCode::BAD_REQUEST,
            "index_not_found",
            "The project root does not contain index.html",
        );
    }

    let destination = state.webapps_root.join(&project);
    let staging = state.webapps_root.join(format!(
        ".publish-{project}-{}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    match tokio::task::spawn_blocking(move || {
        publish_project_tree(&project_path, &destination, &staging)
    })
    .await
    {
        Ok(Ok(files)) => Json(json!({
            "project": project,
            "path": format!("/apps/{project}/"),
            "files": files
        }))
        .into_response(),
        Ok(Err(error)) => api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "project_publish_failed",
            &error.to_string(),
        ),
        Err(error) => api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "project_publish_failed",
            &error.to_string(),
        ),
    }
}

async fn web_app(Path(path): Path<String>, State(state): State<AppState>) -> Response {
    let relative = FsPath::new(&path);
    if !safe_webapp_path(relative) {
        return StatusCode::NOT_FOUND.into_response();
    }

    let root = match tokio::fs::canonicalize(state.webapps_root.as_ref()).await {
        Ok(root) => root,
        Err(_) => return StatusCode::NOT_FOUND.into_response(),
    };
    let mut candidate = state.webapps_root.join(relative);
    if tokio::fs::metadata(&candidate)
        .await
        .is_ok_and(|metadata| metadata.is_dir())
    {
        candidate.push("index.html");
    }
    let candidate = match tokio::fs::canonicalize(candidate).await {
        Ok(candidate) if candidate.starts_with(&root) => candidate,
        _ => return StatusCode::NOT_FOUND.into_response(),
    };
    let metadata = match tokio::fs::metadata(&candidate).await {
        Ok(metadata) if metadata.is_file() => metadata,
        _ => return StatusCode::NOT_FOUND.into_response(),
    };
    const MAX_WEBAPP_FILE_SIZE: u64 = 64 * 1024 * 1024;
    if metadata.len() > MAX_WEBAPP_FILE_SIZE {
        return StatusCode::PAYLOAD_TOO_LARGE.into_response();
    }
    let content = match tokio::fs::read(&candidate).await {
        Ok(content) => content,
        Err(_) => return StatusCode::NOT_FOUND.into_response(),
    };
    let content_type = mime_guess::from_path(&candidate).first_or_octet_stream();

    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", content_type.as_ref())
        .header("x-content-type-options", "nosniff")
        .body(Body::from(content))
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

fn safe_webapp_path(path: &FsPath) -> bool {
    path.components()
        .all(|component| matches!(component, Component::Normal(_)))
}

async fn history_sessions(State(state): State<AppState>) -> Response {
    let sessions = match state.harness.list_stored_sessions(200).await {
        Ok(sessions) => sessions,
        Err(error) => {
            return api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "history_unavailable",
                &error.to_string(),
            );
        }
    };
    let mut sessions = sessions
        .into_iter()
        .map(|session| {
            let rollout_modified = std::fs::metadata(&session.rollout_path)
                .and_then(|metadata| metadata.modified())
                .ok()
                .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|duration| duration.as_secs())
                .unwrap_or_default();
            let modified =
                rollout_modified.max(u64::try_from(session.updated_at_epoch).unwrap_or_default());
            json!({
                "sessionKey": session.session_key,
                "conversationId": session.conversation_id,
                "threadId": session.thread_id,
                "createdAt": session.created_at,
                "modifiedAt": modified
            })
        })
        .collect::<Vec<_>>();
    sessions.sort_by_key(|session| std::cmp::Reverse(session["modifiedAt"].as_u64()));
    Json(json!({"sessions": sessions})).into_response()
}

async fn project_history(Path(project): Path<String>, State(state): State<AppState>) -> Response {
    let project = match resolve_project(&state, Some(&project)).await {
        Ok(Some((name, _))) => name,
        Ok(None) => {
            return api_error(
                StatusCode::NOT_FOUND,
                "projects_disabled",
                "Project management is disabled",
            );
        }
        Err(error) => {
            return api_error(
                StatusCode::NOT_FOUND,
                "project_not_found",
                &error.to_string(),
            );
        }
    };
    let conversation_id = match state.harness.project_conversation_id(&project).await {
        Ok(conversation_id) => conversation_id,
        Err(error) => {
            return api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "history_unavailable",
                &error.to_string(),
            );
        }
    };
    let session_key = Harness::session_key(&conversation_id);
    session_history_response(&state, &session_key, Some(&conversation_id)).await
}

async fn session_history(
    Path(session_key): Path<String>,
    State(state): State<AppState>,
) -> Response {
    session_history_response(&state, &session_key, None).await
}

async fn session_history_response(
    state: &AppState,
    session_key: &str,
    missing_conversation_id: Option<&str>,
) -> Response {
    let session = match state.harness.stored_session(&session_key).await {
        Ok(Some(session)) => session,
        Ok(None) if missing_conversation_id.is_some() => {
            return Json(json!({
                "session": {
                    "sessionKey": session_key,
                    "conversationId": missing_conversation_id,
                    "threadId": Value::Null,
                    "createdAt": Value::Null
                },
                "items": []
            }))
            .into_response();
        }
        Ok(None) => {
            return api_error(
                StatusCode::NOT_FOUND,
                "session_not_found",
                "Session not found",
            );
        }
        Err(error) => {
            return api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "history_unavailable",
                &error.to_string(),
            );
        }
    };
    let paths = vec![session.rollout_path.clone()];
    let mut items = match tokio::task::spawn_blocking(move || {
        let mut items = Vec::new();
        for path in paths {
            if path.exists() {
                items.extend(read_history(&path)?);
            }
        }
        Ok::<_, anyhow::Error>(items)
    })
    .await
    {
        Ok(Ok(items)) => items,
        Ok(Err(error)) => {
            return api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "history_unavailable",
                &error.to_string(),
            );
        }
        Err(error) => {
            return api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "history_unavailable",
                &error.to_string(),
            );
        }
    };
    match state.harness.compaction_history(session_key).await {
        Ok(mut events) => items.append(&mut events),
        Err(error) => return api_error(StatusCode::INTERNAL_SERVER_ERROR, "history_unavailable", &error.to_string()),
    }
    items.sort_by(|left, right| {
        left.get("timestamp")
            .and_then(Value::as_str)
            .cmp(&right.get("timestamp").and_then(Value::as_str))
    });
    Json(json!({
        "session": {
            "sessionKey": session.session_key,
            "conversationId": session.conversation_id,
            "threadId": session.thread_id,
            "createdAt": session.created_at
        },
        "items": items
    }))
    .into_response()
}

async fn agent_websocket(websocket: WebSocketUpgrade, State(state): State<AppState>) -> Response {
    websocket.on_upgrade(move |socket| serve_agent_socket(socket, state))
}

async fn serve_agent_socket(mut socket: WebSocket, state: AppState) {
    if send_ws(&mut socket, json!({"type": "connected"}))
        .await
        .is_err()
    {
        return;
    }

    while let Some(Ok(message)) = socket.recv().await {
        let Message::Text(text) = message else {
            if matches!(message, Message::Close(_)) {
                return;
            }
            continue;
        };
        let request = match serde_json::from_str::<AgentWsMessage>(text.as_ref()) {
            Ok(AgentWsMessage::Turn(request)) => request,
            Ok(AgentWsMessage::Control(AgentWsControl::TurnInterrupt)) => {
                let _ = send_ws(
                    &mut socket,
                    json!({"type": "error", "message": "No active turn"}),
                )
                .await;
                continue;
            }
            Ok(AgentWsMessage::Control(AgentWsControl::TurnSteer { .. })) => {
                let _ = send_ws(
                    &mut socket,
                    json!({"type": "turn.steer.rejected", "message": "No active turn"}),
                )
                .await;
                continue;
            }
            Err(error) => {
                let _ = send_ws(
                    &mut socket,
                    json!({"type": "error", "message": format!("Invalid request: {error}")}),
                )
                .await;
                continue;
            }
        };
        if (!request.compact && request.text.trim().is_empty()) || request.session_id.trim().is_empty() {
            let _ = send_ws(
                &mut socket,
                json!({"type": "error", "message": "Session and message are required"}),
            )
            .await;
            continue;
        }

        if send_ws(&mut socket, json!({"type": "turn.queued"}))
            .await
            .is_err()
        {
            return;
        }
        if let Err(error) = run_web_turn(&mut socket, &state, request).await {
            if send_ws(&mut socket, json!({"type":"error","message":error.to_string()})).await.is_err() { return; }
        }
    }
}

async fn run_web_turn(
    socket: &mut WebSocket,
    state: &AppState,
    request: AgentWsRequest,
) -> anyhow::Result<()> {
    if request.compact {
        return run_web_compaction(socket, state, request).await;
    }
    let project = resolve_project(state, request.project.as_deref()).await?;
    let conversation_id = match project.as_ref() {
        Some((name, _)) => state.harness.project_conversation_id(name).await?,
        None => request.session_id.clone(),
    };
    let preparation_started = Instant::now();
    send_ws(
        socket,
        json!({
            "type": "item.started",
            "id": "control-session-prepare",
            "kind": "control",
            "title": "セッションを準備"
        }),
    )
    .await?;
    let session_key = Harness::session_key(&conversation_id);
    let session_permit = state.harness.acquire_session(&session_key).await?;
    let prepared = state
        .harness
        .prepare_turn(
            &session_key,
            &conversation_id,
            request.reset,
            request.text,
            None,
            project.as_ref().map(|(_, path)| path.as_path()),
        )
        .await?;
    let preparation_ms =
        i64::try_from(preparation_started.elapsed().as_millis()).unwrap_or(i64::MAX);
    send_ws(
        socket,
        json!({
            "type": "item.completed",
            "id": "control-session-prepare",
            "kind": "control",
            "title": "セッションを準備",
            "status": "completed",
            "durationMs": preparation_ms
        }),
    )
    .await?;
    let (chat_tx, mut chat_rx) = mpsc::channel::<String>(64);
    let (event_tx, mut event_rx) = mpsc::channel::<Value>(64);
    let response_id = format!("chatcmpl-web-{}", Uuid::new_v4().simple());
    let active_thread = Arc::clone(&prepared.thread);
    let harness = Arc::clone(&state.harness);
    let model = state.model.to_string();
    tokio::spawn(async move {
        harness
            .run_turn(
                session_permit,
                prepared.thread,
                prepared.prompt,
                prepared.turn_instructions,
                chat_tx,
                response_id,
                model,
                true,
                Some(event_tx),
            )
            .await;
    });

    let mut chat_open = true;
    let mut events_open = true;
    let mut interrupt_requested = false;
    let mut active_turn_id: Option<String> = None;
    while chat_open || events_open {
        tokio::select! {
            event = event_rx.recv(), if events_open => match event {
                Some(event) => {
                    if event.get("type").and_then(Value::as_str) == Some("turn.ready") {
                        active_turn_id = event.get("turnId").and_then(Value::as_str).map(str::to_owned);
                    }
                    send_ws(socket, event).await?;
                }
                None => events_open = false,
            },
            chunk = chat_rx.recv(), if chat_open => match chunk {
                Some(chunk) => {
                    if let Some(payload) = sse_payload(&chunk)
                        && let Some(error) = payload.get("error")
                    {
                        send_ws(socket, json!({"type": "error", "message": error})).await?;
                    }
                }
                None => chat_open = false,
            },
            message = socket.recv() => match message {
                Some(Ok(Message::Text(text))) => {
                    match serde_json::from_str::<AgentWsMessage>(text.as_ref()) {
                        Ok(AgentWsMessage::Control(AgentWsControl::TurnInterrupt)) => {
                            if !interrupt_requested {
                                interrupt_requested = true;
                                send_ws(socket, json!({"type": "turn.interrupting"})).await?;
                                state.harness.interrupt_turn(&active_thread).await?;
                            }
                        }
                        Ok(AgentWsMessage::Control(AgentWsControl::TurnSteer { text })) => {
                            let result = match active_turn_id.as_ref() {
                                Some(turn_id) if !text.trim().is_empty() && !interrupt_requested => {
                                    state.harness.steer_turn(&active_thread, turn_id.clone(), text).await
                                }
                                _ => Err(anyhow::anyhow!("No active turn is ready for additional input")),
                            };
                            match result {
                                Ok(()) => send_ws(socket, json!({"type": "turn.steer.accepted"})).await?,
                                Err(error) => send_ws(socket, json!({"type": "turn.steer.rejected", "message": error.to_string()})).await?,
                            }
                        }
                        Ok(AgentWsMessage::Turn(_)) => {
                            send_ws(socket, json!({"type": "error", "message": "A turn is already running"})).await?;
                        }
                        Err(error) => {
                            send_ws(socket, json!({"type": "error", "message": format!("Invalid request: {error}")})).await?;
                        }
                    }
                }
                Some(Ok(Message::Close(_))) | None => {
                    let _ = state.harness.interrupt_turn(&active_thread).await;
                    return Ok(());
                }
                Some(Err(error)) => {
                    let _ = state.harness.interrupt_turn(&active_thread).await;
                    return Err(error.into());
                }
                Some(Ok(_)) => {}
            },
        }
    }
    Ok(())
}

async fn run_web_compaction(socket: &mut WebSocket, state: &AppState, request: AgentWsRequest) -> anyhow::Result<()> {
    let project = resolve_project(state, request.project.as_deref()).await?;
    let conversation_id = match project.as_ref() {
        Some((name, _)) => state.harness.project_conversation_id(name).await?,
        None => request.session_id,
    };
    let session_key = Harness::session_key(&conversation_id);
    let _permit = state.harness.acquire_session(&session_key).await?;
    let thread = state.harness.compaction_thread(&session_key, &conversation_id, project.as_ref().map(|(_, path)| path.as_path())).await?;
    let (tx, mut rx) = mpsc::channel(64);
    let operation = state.harness.compact_session(&thread, &session_key, &tx);
    tokio::pin!(operation);
    let mut connected = true;
    loop {
        tokio::select! {
            result = &mut operation => {
                while let Ok(event) = rx.try_recv() {
                    if connected { send_ws(socket, event).await?; }
                }
                if connected {
                    match result {
                        Ok(event) => send_ws(socket, event).await?,
                        Err(error) => send_ws(socket, json!({"type":"error","message":error.to_string()})).await?,
                    }
                }
                return Ok(());
            }
            Some(event) = rx.recv() => {
                if connected && send_ws(socket, event).await.is_err() {
                    connected = false;
                    state.harness.interrupt_turn(&thread).await?;
                }
            }
            message = socket.recv(), if connected => match message {
                Some(Ok(Message::Text(text))) => match serde_json::from_str::<AgentWsMessage>(text.as_ref()) {
                    Ok(AgentWsMessage::Control(AgentWsControl::TurnInterrupt)) => {
                        send_ws(socket, json!({"type":"turn.interrupting"})).await?;
                        state.harness.interrupt_turn(&thread).await?;
                    }
                    _ => { send_ws(socket, json!({"type":"turn.steer.rejected","message":"コンテキスト整理中は追加入力できません。"})).await?; }
                },
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => {
                    connected = false;
                    state.harness.interrupt_turn(&thread).await?;
                }
                _ => {}
            }
        }
    }
}

async fn send_ws(socket: &mut WebSocket, payload: Value) -> anyhow::Result<()> {
    socket
        .send(Message::Text(payload.to_string().into()))
        .await
        .map_err(Into::into)
}

async fn models(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if !authorized(&headers, &state.api_key) {
        return api_error(
            StatusCode::UNAUTHORIZED,
            "invalid_api_key",
            "Invalid API key",
        );
    }
    Json(json!({
        "object": "list",
        "data": [{
            "id": state.model.as_str(),
            "object": "model",
            "owned_by": "local-codex-harness"
        }]
    }))
    .into_response()
}

async fn chat_completions(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<ChatRequest>,
) -> Response {
    if !authorized(&headers, &state.api_key) {
        return api_error(
            StatusCode::UNAUTHORIZED,
            "invalid_api_key",
            "Invalid API key",
        );
    }
    let stream_response = request.stream.unwrap_or(false);
    let Some(turn_input) = chat_turn_input(&request.messages) else {
        return api_error(
            StatusCode::BAD_REQUEST,
            "missing_user_message",
            "A user message is required",
        );
    };
    let meta = request.meta.unwrap_or_default();
    let include_metrics = meta.include_metrics;
    let conversation_id = meta
        .session_key
        .or(request.user)
        .unwrap_or_else(|| Uuid::new_v4().to_string());
    let session_key = Harness::session_key(&conversation_id);
    let session_permit = match state.harness.acquire_session(&session_key).await {
        Ok(permit) => permit,
        Err(error) => {
            return api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "session_queue_unavailable",
                &error.to_string(),
            );
        }
    };

    let prepared = match state
        .harness
        .prepare_turn(
            &session_key,
            &conversation_id,
            meta.reset_session,
            turn_input.prompt,
            turn_input.instructions,
            None,
        )
        .await
    {
        Ok(prepared) => prepared,
        Err(error) => {
            return api_error(
                StatusCode::BAD_GATEWAY,
                "harness_unavailable",
                &error.to_string(),
            );
        }
    };

    let (tx, rx) = mpsc::channel::<String>(64);
    let response_id = format!("chatcmpl-local-{}", Uuid::new_v4().simple());
    let harness = Arc::clone(&state.harness);
    let model = state.model.to_string();
    let turn_response_id = response_id.clone();
    let turn_model = model.clone();
    tokio::spawn(async move {
        harness
            .run_turn(
                session_permit,
                prepared.thread,
                prepared.prompt,
                prepared.turn_instructions,
                tx,
                turn_response_id,
                turn_model,
                include_metrics,
                None,
            )
            .await;
    });

    if !stream_response {
        return collect_completion(rx, response_id, model).await;
    }

    let stream = ReceiverStream::new(rx).map(|chunk| Ok::<Bytes, Infallible>(Bytes::from(chunk)));
    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "text/event-stream")
        .header("cache-control", "no-cache")
        .header("x-accel-buffering", "no")
        .body(Body::from_stream(stream))
        .expect("valid SSE response")
}

async fn collect_completion(
    mut rx: mpsc::Receiver<String>,
    response_id: String,
    model: String,
) -> Response {
    let mut content = String::new();
    let mut finish_reason = None;
    let mut usage = Value::Null;
    let mut metrics = Value::Null;
    let mut codexagent = json!({"skills": []});

    while let Some(chunk) = rx.recv().await {
        let Some(payload) = sse_payload(&chunk) else {
            continue;
        };
        if let Some(error) = payload.get("error") {
            return (StatusCode::BAD_GATEWAY, Json(json!({"error": error}))).into_response();
        }
        if let Some(text) = payload
            .pointer("/choices/0/delta/content")
            .and_then(Value::as_str)
        {
            content.push_str(text);
        }
        if let Some(reason) = payload
            .pointer("/choices/0/finish_reason")
            .and_then(Value::as_str)
        {
            finish_reason = Some(reason.to_string());
        }
        if let Some(value) = payload.get("usage") {
            usage = value.clone();
        }
        if let Some(value) = payload.get("codexagent_metrics") {
            metrics = value.clone();
        }
        if let Some(value) = payload.get("codexagent") {
            codexagent = value.clone();
        }
    }

    let mut response = json!({
        "id": response_id,
        "object": "chat.completion",
        "created": unix_timestamp(),
        "model": model,
        "choices": [{
            "index": 0,
            "message": {
                "role": "assistant",
                "content": content
            },
            "finish_reason": finish_reason.unwrap_or_else(|| "stop".to_string())
        }]
    });
    if !usage.is_null() {
        response["usage"] = usage;
    }
    if !metrics.is_null() {
        response["codexagent_metrics"] = metrics;
    }
    response["codexagent"] = codexagent;
    Json(response).into_response()
}

fn sse_payload(chunk: &str) -> Option<Value> {
    let data = chunk.trim().strip_prefix("data: ")?.trim();
    if data == "[DONE]" {
        return None;
    }
    serde_json::from_str(data).ok()
}

fn unix_timestamp() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn authorized(headers: &HeaderMap, api_key: &str) -> bool {
    headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value == format!("Bearer {api_key}"))
}

fn chat_turn_input(messages: &[ChatMessage]) -> Option<ChatTurnInput> {
    let prompt = messages
        .iter()
        .rev()
        .find(|message| message.role == "user")
        .and_then(|message| content_text(&message.content))
        .filter(|text| !text.trim().is_empty())?;
    let instructions = messages
        .iter()
        .filter(|message| matches!(message.role.as_str(), "system" | "developer"))
        .filter_map(|message| {
            content_text(&message.content)
                .filter(|text| !text.trim().is_empty())
                .map(|text| format!("{}:\n{text}", message.role))
        })
        .collect::<Vec<_>>();

    Some(ChatTurnInput {
        prompt,
        instructions: (!instructions.is_empty()).then(|| instructions.join("\n\n")),
    })
}

fn content_text(content: &Value) -> Option<String> {
    match content {
        Value::String(text) => Some(text.clone()),
        Value::Array(parts) => {
            let text = parts
                .iter()
                .filter_map(|part| part.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n");
            (!text.is_empty()).then_some(text)
        }
        _ => None,
    }
}

fn api_error(status: StatusCode, code: &str, message: &str) -> Response {
    (
        status,
        Json(json!({
            "error": {
                "message": message,
                "type": "invalid_request_error",
                "code": code
            }
        })),
    )
        .into_response()
}

fn required_env(name: &str) -> anyhow::Result<String> {
    let value = std::env::var(name)?;
    if value.trim().is_empty() {
        anyhow::bail!("{name} must not be empty");
    }
    Ok(value)
}

fn env_flag(name: &str) -> bool {
    std::env::var(name).ok().is_some_and(|value| {
        matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        )
    })
}

async fn resolve_project(
    state: &AppState,
    requested: Option<&str>,
) -> anyhow::Result<Option<(String, PathBuf)>> {
    if !state.projects_enabled {
        return Ok(None);
    }
    let name = requested
        .filter(|name| safe_project_name(name))
        .context("select a valid project")?;
    let root = tokio::fs::canonicalize(state.workspace_root.as_ref()).await?;
    let candidate = tokio::fs::canonicalize(root.join(name)).await?;
    if !candidate.starts_with(&root) || !tokio::fs::metadata(&candidate).await?.is_dir() {
        anyhow::bail!("project is outside the workspace");
    }
    Ok(Some((name.to_string(), candidate)))
}

fn safe_project_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('.')
        && !name.contains('/')
        && !name.contains('\\')
        && name != "webapps"
        && FsPath::new(name)
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
        && FsPath::new(name).components().count() == 1
}

fn publish_project_tree(
    source: &FsPath,
    destination: &FsPath,
    staging: &FsPath,
) -> anyhow::Result<usize> {
    remove_path(staging)?;
    std::fs::create_dir_all(staging)?;
    let mut files = 0;
    if let Err(error) = copy_publishable_tree(source, staging, &mut files) {
        let _ = remove_path(staging);
        return Err(error);
    }
    remove_path(destination)?;
    std::fs::rename(staging, destination)?;
    Ok(files)
}

fn copy_publishable_tree(
    source: &FsPath,
    destination: &FsPath,
    files: &mut usize,
) -> anyhow::Result<()> {
    for entry in std::fs::read_dir(source)? {
        let entry = entry?;
        let name = entry.file_name();
        let name_text = name.to_string_lossy();
        if name_text.starts_with('.') || matches!(name_text.as_ref(), "node_modules" | "target") {
            continue;
        }
        let file_type = entry.file_type()?;
        if file_type.is_symlink() {
            continue;
        }
        let target = destination.join(&name);
        if file_type.is_dir() {
            std::fs::create_dir_all(&target)?;
            copy_publishable_tree(&entry.path(), &target, files)?;
        } else if file_type.is_file() {
            std::fs::copy(entry.path(), target)?;
            *files += 1;
        }
    }
    Ok(())
}

fn remove_path(path: &FsPath) -> anyhow::Result<()> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    if metadata.file_type().is_dir() {
        std::fs::remove_dir_all(path)?;
    } else {
        std::fs::remove_file(path)?;
    }
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("install SIGTERM handler")
            .recv()
            .await;
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        () = ctrl_c => {},
        () = terminate => {},
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compaction_request_needs_no_chat_text() {
        let request: AgentWsMessage = serde_json::from_value(json!({
            "sessionId":"project:one", "project":"one", "compact":true
        })).expect("valid compaction request");
        let AgentWsMessage::Turn(request) = request else { panic!("expected session request"); };
        assert!(request.compact);
        assert_eq!(request.project.as_deref(), Some("one"));
        assert!(request.text.is_empty());
        let request: AgentWsRequest = serde_json::from_value(json!({"sessionId":"one","text":"hello"})).unwrap();
        assert!(!request.compact);
    }

    #[test]
    fn parses_chat_completion_sse_payload() {
        let payload = sse_payload(
            "data: {\"choices\":[{\"delta\":{\"content\":\"こんにちは\"},\"finish_reason\":null}]}\n\n",
        )
        .expect("valid payload");

        assert_eq!(
            payload.pointer("/choices/0/delta/content"),
            Some(&json!("こんにちは"))
        );
        assert!(sse_payload("data: [DONE]\n\n").is_none());
    }

    #[test]
    fn extracts_turn_instructions_separately_from_user_input() {
        let messages = vec![
            ChatMessage {
                role: "system".to_string(),
                content: json!("careの発話だけを短く返す"),
            },
            ChatMessage {
                role: "assistant".to_string(),
                content: json!("古い応答"),
            },
            ChatMessage {
                role: "developer".to_string(),
                content: json!("URLは発話しない"),
            },
            ChatMessage {
                role: "user".to_string(),
                content: json!("バックグラウンドcare event: {...}"),
            },
        ];

        assert_eq!(
            chat_turn_input(&messages),
            Some(ChatTurnInput {
                prompt: "バックグラウンドcare event: {...}".to_string(),
                instructions: Some(
                    "system:\ncareの発話だけを短く返す\n\ndeveloper:\nURLは発話しない".to_string(),
                ),
            })
        );
    }

    #[test]
    fn leaves_plain_user_turn_unchanged() {
        let messages = vec![ChatMessage {
            role: "user".to_string(),
            content: json!("こんにちは"),
        }];

        assert_eq!(
            chat_turn_input(&messages),
            Some(ChatTurnInput {
                prompt: "こんにちは".to_string(),
                instructions: None,
            })
        );
    }

    #[test]
    fn accepts_only_relative_webapp_paths() {
        assert!(safe_webapp_path(FsPath::new("clock/index.html")));
        assert!(!safe_webapp_path(FsPath::new("../secrets/key")));
        assert!(!safe_webapp_path(FsPath::new("/data/state")));
    }

    #[test]
    fn accepts_only_direct_project_directories() {
        assert!(safe_project_name("sample-app"));
        assert!(safe_project_name("日本語プロジェクト"));
        assert!(!safe_project_name("../secrets"));
        assert!(!safe_project_name("nested/project"));
        assert!(!safe_project_name("nested\\project"));
        assert!(!safe_project_name(".agents"));
        assert!(!safe_project_name("webapps"));
    }
}
