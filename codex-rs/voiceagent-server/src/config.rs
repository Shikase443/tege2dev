use std::collections::BTreeMap;
use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;

use anyhow::Context;
use codex_core_api::AbsolutePathBuf;
use codex_core_api::AltScreenMode;
use codex_core_api::ApprovalsReviewer;
use codex_core_api::Arg0DispatchPaths;
use codex_core_api::AskForApproval;
use codex_core_api::AuthCredentialsStoreMode;
use codex_core_api::AutoCompactTokenLimitScope;
use codex_core_api::Config;
use codex_core_api::ConfigLayerStack;
use codex_core_api::Constrained;
use codex_core_api::Feature;
use codex_core_api::Features;
use codex_core_api::GhostSnapshotConfig;
use codex_core_api::History;
use codex_core_api::MemoriesConfig;
use codex_core_api::McpServerConfig;
use codex_core_api::ModelAvailabilityNuxConfig;
use codex_core_api::MultiAgentV2Config;
use codex_core_api::Notice;
use codex_core_api::OAuthCredentialsStoreMode;
use codex_core_api::OtelConfig;
use codex_core_api::Permissions;
use codex_core_api::ProjectConfig;
use codex_core_api::RealtimeAudioConfig;
use codex_core_api::RealtimeConfig;
use codex_core_api::SessionPickerViewMode;
use codex_core_api::SqliteConfig;
use codex_core_api::TerminalResizeReflowConfig;
use codex_core_api::ThreadStoreConfig;
use codex_core_api::ToolSuggestConfig;
use codex_core_api::TuiKeymap;
use codex_core_api::TuiNotificationSettings;
use codex_core_api::TuiPetAnchor;
use codex_core_api::UriBasedFileOpener;
use codex_core_api::WebSearchMode;
use codex_core_api::built_in_model_providers;
use codex_core_api::find_codex_home;
use codex_protocol::models::PermissionProfile;
use codex_protocol::permissions::NetworkSandboxPolicy;

const LMSTUDIO_PROVIDER_ID: &str = "lmstudio";
const DEFAULT_MODEL_PROFILE: &str = "gemma4";

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct McpSettings {
    mcp_servers: HashMap<String, McpServerConfig>,
}

fn parse_mcp_settings(contents: &str) -> anyhow::Result<HashMap<String, McpServerConfig>> {
    let settings: McpSettings = serde_json::from_str(contents).context("parse MCP settings JSON")?;
    Ok(settings.mcp_servers)
}

fn load_mcp_settings(codex_home: &Path) -> anyhow::Result<HashMap<String, McpServerConfig>> {
    let configured_path = std::env::var_os("CODEX_AGENT_MCP_CONFIG_FILE");
    let path = configured_path
        .as_ref()
        .map(PathBuf::from)
        .unwrap_or_else(|| codex_home.join("mcp.json"));
    let contents = match std::fs::read_to_string(&path) {
        Ok(contents) => contents,
        Err(error)
            if error.kind() == std::io::ErrorKind::NotFound && configured_path.is_none() =>
        {
            return Ok(HashMap::new());
        }
        Err(error) => return Err(error).with_context(|| format!("read MCP settings from {}", path.display())),
    };
    let servers = parse_mcp_settings(&contents)
        .with_context(|| format!("load MCP settings from {}", path.display()))?;
    tracing::info!(path = %path.display(), server_count = servers.len(), "loaded MCP settings");
    Ok(servers)
}

fn load_model_instructions() -> anyhow::Result<String> {
    let profile = std::env::var("CODEX_AGENT_MODEL_PROFILE")
        .unwrap_or_else(|_| DEFAULT_MODEL_PROFILE.to_string());
    let path = std::env::var("CODEX_AGENT_MODEL_PROMPT")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(format!("/data/prompts/{profile}.md")));

    std::fs::read_to_string(&path)
        .with_context(|| format!("read model instructions from {}", path.display()))
}

fn load_character_instructions() -> anyhow::Result<String> {
    let path = std::env::var("CODEX_AGENT_CHARACTER_PROMPT")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/data/characters/development/AGENTS.md"));

    std::fs::read_to_string(&path)
        .with_context(|| format!("read character instructions from {}", path.display()))
}

pub fn build_config(
    model: String,
    workspace: PathBuf,
    arg0_paths: Arg0DispatchPaths,
) -> anyhow::Result<Config> {
    let codex_home = find_codex_home().context("find Codex home")?;
    let mcp_servers = load_mcp_settings(&codex_home)?;
    let cwd = AbsolutePathBuf::try_from(workspace).context("resolve agent workspace")?;
    let output_workspace = std::env::var("CODEX_AGENT_OUTPUT_WORKSPACE")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/workspace"));
    let output_root = AbsolutePathBuf::try_from(output_workspace.clone())
        .context("resolve agent output workspace")?;
    let model_provider_id = LMSTUDIO_PROVIDER_ID.to_string();
    let model_providers = built_in_model_providers(/*openai_base_url*/ None);
    let mut model_provider = model_providers
        .get(&model_provider_id)
        .context("LM Studio model provider should be available")?
        .clone();
    if let Ok(value) = std::env::var("CODEX_AGENT_STREAM_IDLE_TIMEOUT_MS") {
        model_provider.stream_idle_timeout_ms = Some(
            value
                .parse()
                .context("parse CODEX_AGENT_STREAM_IDLE_TIMEOUT_MS")?,
        );
    }

    let mut workspace_roots = vec![cwd.clone()];
    if output_root != cwd {
        workspace_roots.push(output_root);
    }
    let permission_profile = PermissionProfile::workspace_write_with(
        &[],
        NetworkSandboxPolicy::Enabled,
        /*exclude_tmpdir_env_var*/ false,
        /*exclude_slash_tmp*/ false,
    );
    let mut permissions = Permissions::from_approval_and_profile(
        Constrained::allow_any(AskForApproval::Never),
        Constrained::allow_any(permission_profile),
    )?;
    permissions.set_workspace_roots(workspace_roots.clone());

    let mut config = Config {
        config_layer_stack: ConfigLayerStack::default(),
        startup_warnings: Vec::new(),
        bypass_hook_trust: false,
        model: Some(model),
        service_tier: None,
        review_model: None,
        model_context_window: std::env::var("CODEX_AGENT_CONTEXT_WINDOW")
            .ok()
            .and_then(|value| value.parse().ok()),
        model_auto_compact_token_limit: None,
        model_auto_compact_token_limit_scope: AutoCompactTokenLimitScope::Total,
        model_provider_id,
        model_provider,
        personality: None,
        permissions,
        explicit_permission_profile_mode: true,
        custom_permission_profiles: Vec::new(),
        approvals_reviewer: ApprovalsReviewer::User,
        enforce_residency: Constrained::allow_any(/*initial_value*/ None),
        hide_agent_reasoning: true,
        show_raw_agent_reasoning: false,
        base_instructions: Some(load_model_instructions()?),
        base_instructions_provenance: None,
        developer_instructions: Some(format!(
            "{}\n\n# Artifact workspace\nユーザーが保存先を指定しない成果物ファイルは現在の作業ディレクトリである {} に保存してください。設定、セッション、内部状態の保存先として /data を使わないでください。",
            load_character_instructions()?,
            output_workspace.display()
        )),
        guardian_policy_config: None,
        include_permissions_instructions: false,
        include_apps_instructions: false,
        include_collaboration_mode_instructions: false,
        include_skill_instructions: true,
        skill_max_context_tokens: None,
        orchestrator_skills_enabled: true,
        orchestrator_mcp_enabled: false,
        include_environment_context: false,
        compact_prompt: None,
        notify: None,
        tui_notifications: TuiNotificationSettings::default(),
        animations: false,
        show_tooltips: false,
        model_availability_nux: ModelAvailabilityNuxConfig::default(),
        tui_alternate_screen: AltScreenMode::Never,
        tui_status_line: None,
        tui_status_line_use_colors: false,
        tui_terminal_title: None,
        tui_theme: None,
        tui_raw_output_mode: false,
        tui_pet: None,
        tui_pet_anchor: TuiPetAnchor::Composer,
        terminal_resize_reflow: TerminalResizeReflowConfig::default(),
        tui_keymap: TuiKeymap::default(),
        tui_session_picker_view: SessionPickerViewMode::Dense,
        tui_resume_cwd: None,
        tui_vim_mode_default: false,
        cwd: cwd.clone(),
        workspace_roots,
        workspace_roots_explicit: true,
        cli_auth_credentials_store_mode: AuthCredentialsStoreMode::File,
        mcp_servers: Constrained::allow_any(mcp_servers),
        non_prefixed_mcp_tool_servers: None,
        mcp_oauth_credentials_store_mode: OAuthCredentialsStoreMode::File,
        mcp_oauth_callback_port: None,
        mcp_oauth_callback_url: None,
        model_providers,
        project_doc_max_bytes: 32 * 1024,
        project_doc_fallback_filenames: Vec::new(),
        tool_output_token_limit: None,
        agents_enabled: false,
        agent_max_threads: Some(1),
        agent_default_subagent_model: None,
        agent_default_subagent_reasoning_effort: None,
        agent_interrupt_message_enabled: false,
        agent_max_depth: 0,
        agent_roles: BTreeMap::new(),
        memories: MemoriesConfig::default(),
        sqlite: SqliteConfig::from_sqlite_home(codex_home.clone()),
        log_dir: codex_home.join("log").to_path_buf(),
        codex_home,
        history: History::default(),
        ephemeral: false,
        extra_config: None,
        file_opener: UriBasedFileOpener::None,
        codex_self_exe: arg0_paths.codex_self_exe,
        codex_linux_sandbox_exe: arg0_paths.codex_linux_sandbox_exe,
        main_execve_wrapper_exe: arg0_paths.main_execve_wrapper_exe,
        zsh_path: None,
        model_reasoning_effort: None,
        plan_mode_reasoning_effort: None,
        model_reasoning_summary: None,
        model_catalog: None,
        model_verbosity: None,
        chatgpt_base_url: "http://127.0.0.1/disabled".to_string(),
        respect_system_proxy: false,
        apps_mcp_product_sku: None,
        responses_api_metadata: BTreeMap::new(),
        realtime_audio: RealtimeAudioConfig::default(),
        experimental_realtime_ws_base_url: None,
        experimental_realtime_webrtc_call_base_url: None,
        experimental_realtime_ws_model: None,
        realtime: RealtimeConfig::default(),
        experimental_realtime_ws_backend_prompt: None,
        experimental_realtime_ws_startup_context: None,
        experimental_realtime_start_instructions: None,
        experimental_thread_store: ThreadStoreConfig::Local,
        forced_chatgpt_workspace_id: None,
        forced_login_method: None,
        web_search_mode: Constrained::allow_any(WebSearchMode::Disabled),
        web_search_config: None,
        experimental_request_user_input_enabled: false,
        update_plan_enabled: false,
        tool_registry: Default::default(),
        code_mode: Default::default(),
        background_terminal_max_timeout: 60_000,
        ghost_snapshot: GhostSnapshotConfig::default(),
        multi_agent_v2: MultiAgentV2Config::default(),
        max_goal_token_budget: None,
        token_budget: None,
        rollout_budget: None,
        current_time_reminder: None,
        features: Default::default(),
        suppress_unstable_features_warning: true,
        active_project: ProjectConfig { trust_level: None },
        notices: Notice::default(),
        check_for_update_on_startup: false,
        disable_paste_burst: false,
        analytics_enabled: Some(false),
        feedback_enabled: false,
        tool_suggest: ToolSuggestConfig::default(),
        otel: OtelConfig::default(),
    };
    config
        .features
        .set(Features::with_defaults())
        .context("configure default features")?;
    config
        .features
        .disable(Feature::ViewImage)
        .context("disable built-in view_image")?;
    Ok(config)
}

pub fn config_for_workspace(base: &Config, workspace: PathBuf) -> anyhow::Result<Config> {
    let cwd = AbsolutePathBuf::try_from(workspace.clone()).context("resolve project workspace")?;
    let workspace_roots = vec![cwd.clone()];
    let mut config = base.clone();
    config.cwd = cwd;
    config.workspace_roots = workspace_roots.clone();
    config.workspace_roots_explicit = true;
    config.permissions.set_workspace_roots(workspace_roots);
    config.developer_instructions = Some(format!(
        "{}\n\n# Artifact workspace\nユーザーが保存先を指定しない成果物ファイルは現在の作業ディレクトリである {} に保存してください。設定、セッション、内部状態の保存先として /data を使わないでください。",
        load_character_instructions()?,
        workspace.display()
    ));
    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::parse_mcp_settings;

    #[test]
    fn loads_http_mcp_server() {
        let servers = parse_mcp_settings(
            r#"{"mcp_servers":{"playwright":{"url":"http://localhost:8931/mcp"}}}"#,
        ).unwrap();
        assert!(servers["playwright"].enabled);
        assert_eq!(serde_json::to_value(&servers["playwright"]).unwrap()["url"], "http://localhost:8931/mcp");
    }

    #[test]
    fn loads_stdio_and_disabled_servers() {
        let servers = parse_mcp_settings(
            r#"{"mcp_servers":{"example":{"command":"example-mcp","args":["--test"],"enabled":false}}}"#,
        ).unwrap();
        assert!(!servers["example"].enabled);
        assert_eq!(serde_json::to_value(&servers["example"]).unwrap()["command"], "example-mcp");
    }

    #[test]
    fn accepts_empty_server_list() {
        assert!(parse_mcp_settings(r#"{"mcp_servers":{}}"#).unwrap().is_empty());
    }

    #[test]
    fn rejects_invalid_settings() {
        assert!(parse_mcp_settings("not JSON").is_err());
        assert!(parse_mcp_settings(r#"{"mcpServers":{}}"#).is_err());
        assert!(parse_mcp_settings(r#"{"mcp_servers":{"broken":{}}}"#).is_err());
        assert!(parse_mcp_settings(r#"{"mcp_servers":{"broken":{"url":"http://localhost","command":"example"}}}"#).is_err());
    }
}
