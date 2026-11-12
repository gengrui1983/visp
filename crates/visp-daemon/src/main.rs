mod config;
mod server;
mod service;
mod shutdown;

use arc_swap::ArcSwap;
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

use visp_core::{
    agent::{AgentConfig, AgentTool, Envelope},
    agent_registry::AgentRegistry,
    context::ContextTrimmer,
    provider::LlmProvider,
    rules::RuleEngine,
    session::{InMemorySessionStore, SessionManager, SessionStatus, SessionStore},
    tool_registry::ToolRegistry,
};

use visp_agent::orchestrator::{CancelSignal, Orchestrator};
use visp_mcp::manager::McpManager;
use visp_tools::{
    bash::Bash,
    codegraph::{
        CodeGraphContext, CodeGraphGetDetails, CodeGraphImpact, CodeGraphRebuild, CodeGraphSearch,
        CodeGraphTrace,
    },
    fetch::WebFetch,
    file::{EditFile, ReadFile, WriteFile},
    search::{Glob, Grep},
};

use crate::config::{DaemonConfig, LlmModelConfig};
use crate::service::CoderDaemonService;

/// Register agent tools from the AgentRegistry into the ToolRegistry.
/// In single-agent mode (global_tx is None), agent tools are skipped.
fn register_agent_tools(
    tool_registry: &ToolRegistry,
    agent_registry: &AgentRegistry,
    global_tx: Option<&mpsc::Sender<Envelope>>,
) -> Result<(), String> {
    if global_tx.is_none() {
        return Ok(());
    }
    for agent_def in agent_registry.list_subagents() {
        let tool = Arc::new(AgentTool::new(
            agent_def.name.clone(),
            agent_def.description.clone(),
        ));
        tool_registry
            .register(tool)
            .map_err(|e| format!("register agent tool '{}': {e}", agent_def.name))?;
    }
    Ok(())
}

/// Create an LLM provider from a model config.
fn create_llm_provider(config: &LlmModelConfig) -> Result<Arc<dyn LlmProvider>, String> {
    match config.protocol.as_str() {
        "openai" => {
            let api_key = config.api_key.clone().ok_or_else(|| {
                "OPENAI_API_KEY not set (configure api_key or set env)".to_string()
            })?;
            if let Some(ref base_url) = config.base_url {
                Ok(Arc::new(visp_llm::openai::OpenAiProvider::with_base_url(
                    api_key,
                    base_url.clone(),
                )))
            } else {
                Ok(Arc::new(visp_llm::openai::OpenAiProvider::new(api_key)))
            }
        }
        "aliyun" => {
            let api_key = config.api_key.clone().ok_or_else(|| {
                "ALIYUN_API_KEY not set (configure api_key or set env)".to_string()
            })?;
            let base_url = config.base_url.clone().ok_or_else(|| {
                "ALIYUN base_url not set (configure base_url in daemon.toml)".to_string()
            })?;
            Ok(Arc::new(visp_llm::aliyun::AliyunProvider::new(
                api_key, base_url,
            )))
        }
        "bigmodel" => {
            let api_key = config.api_key.clone().ok_or_else(|| {
                "BIGMODEL_API_KEY not set (configure api_key or set env)".to_string()
            })?;
            Ok(Arc::new(visp_llm::bigmodel::BigModelProvider::new(
                api_key,
                config.base_url.clone(),
            )))
        }
        "opencode" | "opencode-go" => {
            let api_key = config.api_key.clone().ok_or_else(|| {
                "OPENCODE_API_KEY not set (configure api_key or set env)".to_string()
            })?;
            if let Some(ref base_url) = config.base_url {
                Ok(Arc::new(visp_llm::opencode::OpencodeProvider::new(
                    api_key,
                    Some(base_url.clone()),
                )))
            } else if config.protocol == "opencode-go" {
                // "opencode-go" 默认指向 Go 通道 v1 端点
                Ok(Arc::new(visp_llm::opencode_go::OpencodeGoProvider::new(
                    api_key, None,
                )))
            } else {
                Ok(Arc::new(visp_llm::opencode::OpencodeProvider::new(
                    api_key, None,
                )))
            }
        }
        _ => {
            let api_key = config.api_key.clone().ok_or_else(|| {
                "ANTHROPIC_API_KEY not set (configure api_key or set env)".to_string()
            })?;
            if let Some(ref base_url) = config.base_url {
                Ok(Arc::new(
                    visp_llm::anthropic::AnthropicProvider::with_base_url(
                        api_key,
                        base_url.clone(),
                    ),
                ))
            } else {
                Ok(Arc::new(visp_llm::anthropic::AnthropicProvider::new(
                    api_key,
                )))
            }
        }
    }
}

/// Scan command-line arguments for `--config-dir <value>`.
///
/// Returns the value following `--config-dir`, `None` if the flag is absent or
/// has no value following it.
fn parse_config_dir_from(args: &[String]) -> Option<String> {
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if arg == "--config-dir" {
            return iter.next().cloned();
        }
    }
    None
}

/// Read the `--config-dir` override from the real process arguments.
fn parse_config_dir() -> Option<String> {
    parse_config_dir_from(&std::env::args().collect::<Vec<_>>())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // 0. If --config-dir is given, export it as VISP_CONFIG_DIR so that all
    //    global path functions (daemon.toml, rules, skills, agents, etc.)
    //    resolve under the user-specified directory instead of ~/.config/visp.
    //    This must happen before any config loading.
    if let Some(dir) = parse_config_dir() {
        // SAFETY: single-threaded setup before any threads are spawned.
        unsafe { std::env::set_var("VISP_CONFIG_DIR", &dir) };
        tracing::info!(config_dir = %dir, "using custom config directory");
    }

    // 1. Initialize global config if not exists
    visp_config::init_config().map_err(|e| format!("init config: {e}"))?;

    // 2. Load config
    let config_path = {
        let args: Vec<String> = std::env::args().skip(1).collect();
        let mut config_path = None;
        let mut skip_next = false;
        for arg in &args {
            if skip_next {
                skip_next = false;
                continue;
            }
            if arg.starts_with("--") {
                // Skip the flag and its value
                skip_next = true;
                continue;
            }
            config_path = Some(std::path::PathBuf::from(arg));
            break;
        }
        config_path
    };
    let config: DaemonConfig =
        visp_config::load_config(config_path.as_deref()).map_err(|e| format!("config: {e}"))?;

    // 2. Init observability (tracing subscriber stack)
    //    Guard lives for the lifetime of main; on drop it unwinds the subscriber.
    let _observability_guard = visp_observability::init::init_observability(
        &config.observability,
        &config.daemon.log_level,
    );

    tracing::info!(listen_addr = %config.daemon.listen_addr, "starting visp-daemon");

    // 3. Build model configs
    let model_configs = config.llm.effective_models();
    if model_configs.is_empty() {
        // Regenerate config with a placeholder model so the user has a template to edit
        let mut template_config = config;
        template_config.llm.models = vec![LlmModelConfig {
            name: "claude-sonnet-4-20250514".into(),
            protocol: "anthropic".into(),
            provider: Some("Anthropic".into()),
            model: "claude-sonnet-4-20250514".into(),
            api_key: Some("YOUR_API_KEY".into()),
            base_url: None,
            temperature: None,
            max_tokens: None,
            max_context_tokens: None,
            thinking_budget_tokens: None,
            use_tool: None,
            image_generation: None,
            extra: HashMap::new(),
        }];
        template_config.llm.default = Some("Anthropic/claude-sonnet-4-20250514".into());
        let config_path = visp_config::path::daemon_toml_global()
            .ok_or_else(|| "HOME not set".to_string())
            .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;
        let content = toml::to_string_pretty(&template_config)
            .map_err(|e| -> Box<dyn std::error::Error> { e.to_string().into() })?;
        let tmp = config_path.with_extension("toml.visp-tmp");
        std::fs::write(&tmp, &content)
            .map_err(|e| -> Box<dyn std::error::Error> { e.to_string().into() })?;
        std::fs::rename(&tmp, &config_path)
            .map_err(|e| -> Box<dyn std::error::Error> { e.to_string().into() })?;

        // Write error message to startup-error file for the launcher to display,
        // then exit. stdout/stderr are redirected to a log file by the launcher.
        let msg = format!(
            "No LLM models configured.\n\
             A template config has been written to:\n  {}\n\
             Please edit it with your API key, then restart visp.",
            config_path.display()
        );
        if let Some(err_file) = visp_config::path::startup_error_file() {
            let _ = std::fs::write(&err_file, &msg);
        }
        std::process::exit(1);
    }
    let model_keys: Vec<String> = model_configs.iter().map(|mc| mc.key()).collect();
    let default_protocol = model_configs
        .first()
        .map(|mc| mc.protocol.as_str())
        .unwrap_or("anthropic");
    let available_models = config.llm.available_models();

    tracing::info!(
        protocol = %default_protocol,
        models = %model_keys.join(", "),
        "LLM configured"
    );

    // 4. Create tool registry
    let tool_registry = ToolRegistry::new();

    // ── 常用工具 ──
    tool_registry
        .register(Arc::new(Bash::from_toml(config.tool.get("bash"))))
        .map_err(|e| format!("register bash: {e}"))?;
    tool_registry
        .register(Arc::new(ReadFile::from_toml(config.tool.get("read_file"))))
        .map_err(|e| format!("register read_file: {e}"))?;
    tool_registry
        .register(Arc::new(WriteFile::from_toml(
            config.tool.get("write_file"),
        )))
        .map_err(|e| format!("register write_file: {e}"))?;
    tool_registry
        .register(Arc::new(EditFile::from_toml(config.tool.get("edit_file"))))
        .map_err(|e| format!("register edit_file: {e}"))?;
    tool_registry
        .register(Arc::new(Grep::from_toml(config.tool.get("grep"))))
        .map_err(|e| format!("register grep: {e}"))?;
    tool_registry
        .register(Arc::new(Glob::from_toml(config.tool.get("glob"))))
        .map_err(|e| format!("register glob: {e}"))?;

    // ── 网络工具 ──
    tool_registry
        .register(Arc::new(WebFetch::from_toml(config.tool.get("webfetch"))))
        .map_err(|e| format!("register fetch_web: {e}"))?;

    // ── 代码分析工具 ──
    tool_registry
        .register(Arc::new(CodeGraphSearch::from_toml(
            config.tool.get("codegraph_search"),
        )))
        .map_err(|e| format!("register codegraph_search: {e}"))?;
    tool_registry
        .register(Arc::new(CodeGraphGetDetails::from_toml(
            config.tool.get("codegraph_get_details"),
        )))
        .map_err(|e| format!("register codegraph_get_details: {e}"))?;
    tool_registry
        .register(Arc::new(CodeGraphRebuild))
        .map_err(|e| format!("register codegraph_rebuild: {e}"))?;
    tool_registry
        .register(Arc::new(CodeGraphContext::from_toml(
            config.tool.get("codegraph_context"),
        )))
        .map_err(|e| format!("register codegraph_context: {e}"))?;
    tool_registry
        .register(Arc::new(CodeGraphTrace::from_toml(
            config.tool.get("codegraph_trace"),
        )))
        .map_err(|e| format!("register codegraph_trace: {e}"))?;
    tool_registry
        .register(Arc::new(CodeGraphImpact::from_toml(
            config.tool.get("codegraph_impact"),
        )))
        .map_err(|e| format!("register codegraph_impact: {e}"))?;

    // ── 技能工具（description 内嵌可用技能列表）──
    let cwd = std::env::current_dir()?;
    tool_registry
        .register(Arc::new(visp_tools::skill::SkillTool::new(&cwd)))
        .map_err(|e| format!("register skill: {e}"))?;

    let tool_registry = Arc::new(tool_registry);

    // ── 锁定核心工具（MCP 工具不能覆盖这些名称）──
    tool_registry.seal_core_tools();

    // 5. Initialize MCP Manager
    let mcp_manager = Arc::new(McpManager::new(config.mcp.servers.clone()));
    {
        let tr = tool_registry.clone();
        // 跟踪每个服务器已注册的工具名，用于重连时更新
        let mcp_tool_names: Arc<Mutex<HashMap<String, Vec<String>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let on_ready: visp_mcp::manager::OnToolsReady = Arc::new(move |server_name, tools| {
            let tool_names: Vec<String> = tools.iter().map(|t| t.name().to_string()).collect();

            // 获取已有的该服务器工具名列表（insert 返回旧值）
            let old_tool_names = {
                let mut map = mcp_tool_names.lock().unwrap();
                map.insert(server_name.to_string(), tool_names.clone())
                    .unwrap_or_default()
            };

            // 移除旧工具（第一次注册时 old_tool_names 为空列表，跳过）
            for old_name in &old_tool_names {
                if !tool_names.contains(old_name) {
                    // 工具名已不存在于新列表中，移除
                    let _ = tr.remove(old_name);
                }
            }

            // 注册/更新新工具
            for (i, tool) in tools.into_iter().enumerate() {
                let tool_name = &tool_names[i];
                let tool = Arc::from(tool); // Box → Arc
                if old_tool_names.contains(tool_name) {
                    // 重连场景：更新现有工具
                    if let Err(e) = tr.update(tool_name, tool) {
                        tracing::warn!("failed to update MCP tool '{tool_name}': {e}");
                    }
                } else {
                    // 首次注册场景
                    if let Err(e) = tr.register_mcp(tool) {
                        tracing::warn!("failed to register MCP tool '{tool_name}': {e}");
                    }
                }
            }
        });
        mcp_manager.start_all(on_ready).await;
    }

    // 6. Create rule engine
    let rule_engine = Arc::new(RuleEngine::new(&cwd)?);

    // 6.5. Create context trimmer
    // 无配置项时使用默认值；如需自定义，可通过结构体字面量注入：
    //   DefaultContextTrimmer { head_turns, tail_turns, tool_output_max_chars }
    let context_trimmer: Arc<dyn ContextTrimmer + Send + Sync> =
        Arc::new(visp_context::DefaultContextTrimmer {
            head_turns: 5,
            tail_turns: 10,
            tool_output_max_chars: 8_000,
        });

    // 7. Create session manager
    let store: Box<dyn SessionStore> = match config.storage.driver.as_str() {
        "memory" => Box::new(InMemorySessionStore::new()),
        "sqlite" => {
            let sqlite_store = visp_db::SqliteSessionStore::open(&config.storage.path)
                .map_err(|e| format!("failed to open sqlite store: {e}"))?;
            Box::new(sqlite_store)
        }
        other => {
            return Err(
                format!("unknown storage driver: {other} (expected 'sqlite' or 'memory')").into(),
            );
        }
    };
    let session_mgr = Arc::new(SessionManager::new(store));

    // 7.1. Reset orphaned Running sessions to Idle.
    // When daemon restarts, sessions that were Running in the previous
    // instance have no actual agent loop but retain Running status in DB.
    // Resetting them to Idle ensures they can accept new input.
    if let Ok(sessions) = session_mgr.list() {
        for session in &sessions {
            if session.status == SessionStatus::Running {
                tracing::info!(
                    session_id = %session.id,
                    "resetting orphaned Running session to Idle"
                );
                let _ = session_mgr.finish_loop(&session.id, SessionStatus::Idle);
            }
        }
    }

    // 7.2. Delete empty sessions (no messages).
    // Sessions created but never used (e.g. from aborted /new commands)
    // clutter the session list. Clean them up on startup.
    if let Ok(sessions) = session_mgr.list() {
        for session in &sessions {
            if let Ok(messages) = session_mgr.get_messages(&session.id)
                && messages.is_empty()
            {
                tracing::info!(
                    session_id = %session.id,
                    "deleting empty session (no messages)"
                );
                let _ = session_mgr.delete(&session.id);
            }
        }
    }

    // 8. Agent config
    let langfuse = &config.observability.langfuse;
    let langfuse_tags = if langfuse.tags.is_empty() {
        None
    } else {
        Some(serde_json::to_string(&langfuse.tags).unwrap_or_default())
    };
    let langfuse_metadata = langfuse.metadata.as_ref().map(|meta| {
        meta.iter()
            .map(|(k, v)| {
                let val = match v {
                    toml::Value::String(s) => s.clone(),
                    toml::Value::Integer(i) => i.to_string(),
                    toml::Value::Float(f) => f.to_string(),
                    toml::Value::Boolean(b) => b.to_string(),
                    _ => serde_json::to_string(v).unwrap_or_default(),
                };
                (k.clone(), val)
            })
            .collect()
    });
    let agent_config = AgentConfig {
        soft_limit: config.agent.soft_limit,
        hard_limit: 200,
        doom_loop_threshold: config.agent.doom_loop_threshold,
        llm_retry_attempts: config.agent.llm_retry_attempts,
        llm_retry_base_delay_ms: config.agent.llm_retry_base_delay_ms,
        bash_confirm_mode: config.agent.bash_confirm_mode,
        file_max_size_bytes: config.agent.file_max_size_bytes,
        max_depth: config.agent.max_depth,
        max_concurrent_subagents: config.agent.max_concurrent_subagents,
        langfuse_enabled: langfuse.enabled,
        langfuse_user_id: langfuse.user_id.clone(),
        langfuse_tags,
        langfuse_environment: langfuse.environment.clone(),
        langfuse_release: langfuse.release.clone(),
        langfuse_version: langfuse.version.clone(),
        langfuse_public: langfuse.public,
        langfuse_metadata,
        langfuse_capture_input: langfuse.capture.input,
        langfuse_capture_output: langfuse.capture.output,
        langfuse_capture_max_chars: langfuse.capture.max_chars,
        langfuse_redact_secrets: langfuse.capture.redact_secrets,
    };

    // 8.5. Create provider HashMap
    let mut providers: HashMap<String, Arc<dyn LlmProvider>> = HashMap::new();
    for mc in &model_configs {
        match create_llm_provider(mc) {
            Ok(provider) => {
                let key = mc.key();
                providers.insert(key, provider.clone());
                // 额外注册 {provider}/{model} 别名，使 agent 配置文件中的 model 字段
                // 可以使用更直观的格式，例如 "Opencode/deepseek-v4-flash"
                let provider_name = mc.provider.as_deref().unwrap_or(&mc.protocol);
                let model_alias = format!("{provider_name}/{}", mc.model);
                if model_alias != mc.key() {
                    providers.insert(model_alias, provider);
                }
            }
            Err(e) => {
                tracing::warn!(model_key = %mc.key(), error = %e, "failed to create provider");
            }
        }
    }

    // 8.6. Load agent definitions
    let mut builtin_overrides: Vec<visp_agent::agent_loader::BuiltinAgentOverride> = config
        .agent
        .builtin
        .iter()
        .map(|c| visp_agent::agent_loader::BuiltinAgentOverride {
            name: c.name.clone(),
            model: c.model.clone(),
            temperature: c.temperature,
            steps: c.steps,
        })
        .collect();

    // Wire llm.image_generation_model / llm.vision_model to builtin agents
    if let Some(ref key) = config.llm.image_generation_model {
        builtin_overrides.push(visp_agent::agent_loader::BuiltinAgentOverride {
            name: "painter".to_string(),
            model: Some(key.clone()),
            temperature: None,
            steps: None,
        });
    }
    if let Some(ref key) = config.llm.vision_model {
        builtin_overrides.push(visp_agent::agent_loader::BuiltinAgentOverride {
            name: "vision".to_string(),
            model: Some(key.clone()),
            temperature: None,
            steps: None,
        });
    }

    // Agent directories: global config dir (lower priority) → project dir (higher priority)
    let mut agent_dirs: Vec<std::path::PathBuf> = Vec::new();
    if let Some(global_agents_dir) = visp_config::path::agents_dir_global()
        && global_agents_dir.exists()
    {
        agent_dirs.push(global_agents_dir);
    }
    let project_agents_dir = visp_config::path::agents_dir_project(&cwd);
    if project_agents_dir.exists() {
        agent_dirs.push(project_agents_dir);
    }
    let agent_dir_refs: Vec<&Path> = agent_dirs.iter().map(|p| p.as_path()).collect();
    let agent_registry = Arc::new(ArcSwap::from_pointee(
        visp_agent::agent_loader::load_agents(&agent_dir_refs, &builtin_overrides),
    ));

    // 8.7. Create orchestration channels
    let (global_tx, global_rx) = mpsc::channel(256);
    let (cancel_tx, cancel_rx) = mpsc::channel::<CancelSignal>(16);
    let (client_tx, client_rx) = mpsc::channel(64);

    // 8.7.1. 进程内事件总线：显示面帧的唯一来源（设计 §5 D3）。
    //         orchestrator/reload/watch 发布，每个 Chat 连接独立订阅。
    let bus = Arc::new(visp_daemon::bus::EventBus::new());

    // 8.7.2. hook 运行时接线（设计 §5 D13）：零生效规则 → None（不订阅、零开销）；
    //         有规则 → 订阅总线（Frame 经 hook_map 映射后派发），同一运行时在下方
    //         作为关停 drain 宿主注入 service。订阅先于 orchestrator 启动，避免丢早期帧。
    let hook_trust_store = visp_config::HookTrustStore::load();
    let hook_runtime =
        visp_daemon::hook_runtime::setup_hook_runtime(&config.hooks, &cwd, &hook_trust_store, &bus);

    // 8.7.5. Register agent tools from AgentRegistry (skip in single-agent mode)
    let agent_registry_snapshot = agent_registry.load_full();
    register_agent_tools(&tool_registry, &agent_registry_snapshot, Some(&global_tx))
        .map_err(|e| format!("register agent tools: {e}"))?;

    // 8.8. Create and start Orchestrator
    let mut orchestrator = Orchestrator::new(
        cancel_rx,
        global_rx,
        global_tx.clone(),
        client_rx,
        session_mgr.clone(),
        agent_registry.clone(),
        tool_registry.clone(),
        rule_engine.clone(),
        agent_config.clone(),
        context_trimmer.clone(),
        Arc::new(config.clone()),
        providers,
        bus.clone(),
    )
    .with_verifier_config(visp_agent::verifier::VerifierConfig {
        enabled: config.verifier.is_enabled(),
        max_rounds: config.verifier.effective_max_rounds(),
        model: config.verifier.model.clone(),
        timeout_secs: config.verifier.effective_timeout_secs(),
        max_steps: config.verifier.effective_max_steps(),
    });
    tokio::spawn(async move {
        orchestrator.run().await;
    });

    // 8.9. 共享 reload 核心：显式 `/reload` 与后续自动文件监听复用同一实例
    //      （设计 §5.5；project root 固定为启动时 cwd，agent 目录每次重载重算）。
    let reload_core = Arc::new(
        visp_daemon::reload::ReloadCore::new(
            rule_engine.clone(),
            tool_registry.clone(),
            agent_registry.clone(),
            builtin_overrides.clone(),
            Some(global_tx.clone()),
            cwd.clone(),
        )
        // 通知发布到事件总线（设计 §5 D3）。
        .with_bus(bus.clone()),
    );

    // 9. Assemble service
    let mcp_shutdown = mcp_manager.clone();
    // 优雅关停管道（设计 D13）：Shutdown RPC 经此 Notify 唤醒 main；hook 有界
    // drain 宿主：有 hook 规则时为真实执行器（8.7.2 接线），否则维持空实现。
    let shutdown_notify = Arc::new(tokio::sync::Notify::new());
    let hook_drain: Arc<dyn crate::shutdown::HookDrainHost> = match &hook_runtime {
        Some(runtime) => runtime.clone(),
        None => Arc::new(crate::shutdown::NoopHookDrain),
    };
    // 只读统计来源（GetHookStats）：与 drain 宿主同一运行时；无运行时为全零实现。
    let hook_stats: Arc<dyn visp_daemon::hook_runtime::HookStatsSource> = match &hook_runtime {
        Some(runtime) => runtime.clone(),
        None => Arc::new(visp_daemon::hook_runtime::NoopHookStats),
    };
    let service = CoderDaemonService::new(
        model_configs,
        tool_registry,
        rule_engine,
        reload_core.clone(),
        session_mgr,
        agent_config,
        Arc::new(config.clone()),
        context_trimmer,
        mcp_manager,
        available_models,
        cancel_tx,
        bus.clone(),
        client_tx,
        shutdown_notify.clone(),
        hook_drain,
        hook_stats,
    )
    .inspect_err(|error| {
        // The desktop launcher redirects release stdout/stderr to a log file.
        // Persist the actionable startup error so it can show the user what
        // needs fixing instead of only reporting an exit code.
        if let Some(err_file) = visp_config::path::startup_error_file() {
            let _ = std::fs::write(&err_file, error);
        }
    })?;

    // 9.5. 挂载自动文件监听（设计 §5.6 生命周期）：service 组装完成后、gRPC server
    //      启动前。开关由 daemon.toml 的 [daemon].filewatcher 控制，重启生效
    //      （设计 §7 决策 12）；关闭即自动热重载的唯一回滚入口（设计 §9）。
    let global_config_dir = visp_config::path::global_config_dir();
    let file_watcher = visp_daemon::watch::start_file_watcher(
        config.daemon.filewatcher,
        &cwd,
        global_config_dir.as_deref(),
        Arc::new(visp_daemon::watch::CoreReloadExecutor::new(
            reload_core.clone(),
        )),
        // 细则 5 降级通知发布到事件总线（与 reload 通知同一来源）。
        Some(bus.clone()),
    )
    .await;

    // 10. Start gRPC server
    let addr = config.daemon.listen_addr.clone();
    let server_handle = tokio::spawn(async move {
        if let Err(e) = server::start_server(&addr, service).await {
            tracing::error!("grpc server error: {e}");
        }
    });

    // 11. 等待关停信号：Ctrl+C / Shutdown RPC（shutdown_notify）/ SIGTERM
    //     三者走同一清理路径（设计 D13）。
    let trigger = crate::shutdown::wait_for_shutdown(&shutdown_notify).await?;
    tracing::info!(?trigger, "shutdown signal received, stopping server");

    // 12. 进程级清理：停 watcher → 关 MCP → abort server（三入口共用）。
    crate::shutdown::run_shutdown_cleanup(file_watcher, mcp_shutdown, &server_handle).await;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use visp_core::agent_definition::{AgentDefinition, AgentMode};

    fn make_agent(name: &str, mode: AgentMode) -> AgentDefinition {
        AgentDefinition {
            name: name.to_string(),
            description: format!("Agent {name}"),
            mode,
            model: None,
            temperature: None,
            steps: None,
            permission: vec![],
            allowed_sub_agents: Vec::new(),
            system_prompt: String::new(),
        }
    }

    fn make_registry(subagent_names: &[&str]) -> AgentRegistry {
        let mut registry = AgentRegistry::new();
        for name in subagent_names {
            registry
                .register(make_agent(name, AgentMode::Subagent))
                .unwrap();
        }
        // Also register a primary agent to verify it's not included
        registry
            .register(make_agent("primary", AgentMode::Primary))
            .unwrap();
        registry
    }

    #[test]
    fn test_agent_tools_registered_from_registry() {
        let registry = make_registry(&["fixer", "explorer"]);
        let tool_registry = ToolRegistry::new();
        let (tx, _rx) = mpsc::channel::<Envelope>(16);

        let result = register_agent_tools(&tool_registry, &registry, Some(&tx));
        assert!(result.is_ok());

        let defs = tool_registry.definitions();
        let names: Vec<&str> = defs.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names.len(), 2);
        assert!(names.contains(&"fixer"));
        assert!(names.contains(&"explorer"));
        // Primary agent should NOT be registered
        assert!(!names.contains(&"primary"));
    }

    #[test]
    fn test_agent_tools_not_registered_in_single_agent_mode() {
        let registry = make_registry(&["fixer", "explorer"]);
        let tool_registry = ToolRegistry::new();

        // global_tx is None → single agent mode
        let result = register_agent_tools(&tool_registry, &registry, None);
        assert!(result.is_ok());

        let defs = tool_registry.definitions();
        assert!(
            defs.is_empty(),
            "No agent tools should be registered in single-agent mode"
        );
    }

    #[test]
    fn test_agent_tool_names_match_agent_definition() {
        let registry = make_registry(&["fixer", "explorer", "designer"]);
        let tool_registry = ToolRegistry::new();
        let (tx, _rx) = mpsc::channel::<Envelope>(16);

        let result = register_agent_tools(&tool_registry, &registry, Some(&tx));
        assert!(result.is_ok());

        let defs = tool_registry.definitions();
        for def in &defs {
            // Each registered tool name should match an agent definition name
            let agent = registry.get(&def.name);
            assert!(
                agent.is_some(),
                "Tool '{}' should have a matching AgentDefinition",
                def.name
            );
            assert_eq!(
                agent.unwrap().name,
                def.name,
                "Tool name should match AgentDefinition.name"
            );
        }
        // Verify tool_type for all registered tools
        for def in &defs {
            let tool = tool_registry.get(&def.name).unwrap();
            assert_eq!(
                tool.tool_type(),
                visp_core::tool::ToolType::Agent,
                "Agent tool '{}' should have ToolType::Agent",
                def.name
            );
        }
    }
    #[test]
    fn test_parse_config_dir_arg() {
        let args = vec![
            "visp-daemon".to_string(),
            "--config-dir".to_string(),
            "/custom/dir".to_string(),
        ];
        assert_eq!(
            parse_config_dir_from(&args),
            Some("/custom/dir".to_string())
        );
    }

    #[test]
    fn test_parse_config_dir_missing_value() {
        let args = vec!["visp-daemon".to_string(), "--config-dir".to_string()];
        assert_eq!(parse_config_dir_from(&args), None);
    }

    #[test]
    fn test_parse_config_dir_absent() {
        let args = vec!["visp-daemon".to_string()];
        assert_eq!(parse_config_dir_from(&args), None);
    }

    #[test]
    fn test_parse_config_dir_with_other_flags() {
        let args = vec![
            "visp-daemon".to_string(),
            "--http-addr".to_string(),
            "0.0.0.0:9090".to_string(),
            "--config-dir".to_string(),
            "/custom/dir".to_string(),
            "config.toml".to_string(),
        ];
        assert_eq!(
            parse_config_dir_from(&args),
            Some("/custom/dir".to_string())
        );
    }
}
