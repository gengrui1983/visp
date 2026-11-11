use crate::hooks::{HooksConfig, merge_hooks};
use crate::path;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::path::Path;

/// MCP 配置（daemon.toml 中的 [mcp] section）
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct McpConfig {
    #[serde(default)]
    pub servers: Vec<McpServerConfig>,
}

/// 单个 MCP 服务器配置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpServerConfig {
    /// 唯一标识名
    pub name: String,
    /// 传输方式
    pub transport: McpTransport,
    /// 是否在启动时自动连接（默认 true）
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    /// 工具名称前缀，防止不同服务器的工具名冲突
    #[serde(default)]
    pub tool_prefix: Option<String>,
    /// 工具调用超时秒数（默认 60）
    #[serde(default = "default_tool_timeout")]
    pub tool_timeout_secs: u64,
}

fn default_enabled() -> bool {
    true
}

fn default_tool_timeout() -> u64 {
    60
}

/// MCP 传输方式
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum McpTransport {
    /// 子进程方式：daemon 启动并管理
    #[serde(rename = "stdio")]
    Stdio {
        /// 启动命令
        command: String,
        /// 命令参数
        #[serde(default)]
        args: Vec<String>,
        /// 环境变量（继承 daemon 环境，此处的为叠加/覆盖）
        #[serde(default)]
        env: HashMap<String, String>,
    },
    /// HTTP SSE 方式：连接已运行的 MCP 服务器
    #[serde(rename = "sse")]
    Sse {
        /// SSE 端点 URL
        url: String,
        /// HTTP 请求头（用于认证等）
        #[serde(default)]
        headers: HashMap<String, String>,
    },
    /// 简易 HTTP GET 方式：用于只接受 GET 请求的简易服务
    ///
    /// 不经过 MCP 协议握手，直接通过 GET 请求发现工具和调用工具。
    /// - 工具发现：`GET {url}{tools_endpoint}`（默认 `/tools`）
    /// - 工具调用：`GET {url}{call_endpoint}?name={tool_name}&{query_params}`
    #[serde(rename = "get")]
    Get {
        /// 基础 URL
        url: String,
        /// HTTP 请求头（用于认证等）
        #[serde(default)]
        headers: HashMap<String, String>,
        /// 工具列表端点，相对于 base_url（默认 "/tools"）
        #[serde(default = "default_tools_endpoint")]
        tools_endpoint: String,
        /// 工具调用端点，相对于 base_url（默认 "/call"）
        #[serde(default = "default_call_endpoint")]
        call_endpoint: String,
    },
    /// HTTP Streamable 方式：通过单一 POST 端点完成 MCP 协议
    ///
    /// 遵循 MCP Streamable HTTP 规范，所有交互通过 POST 完成。
    /// 适用于 context7 等只接受 POST 的 MCP 服务器。
    /// - 初始化：`POST {url}` → initialize
    /// - 工具发现：`POST {url}` → tools/list
    /// - 工具调用：`POST {url}` → tools/call
    #[serde(rename = "http")]
    Http {
        /// 端点 URL
        url: String,
        /// HTTP 请求头（用于认证等）
        #[serde(default)]
        headers: HashMap<String, String>,
    },
}

fn default_tools_endpoint() -> String {
    "/tools".to_string()
}

fn default_call_endpoint() -> String {
    "/call".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonConfig {
    #[serde(default = "default_daemon_section")]
    pub daemon: DaemonSection,
    #[serde(default)]
    pub llm: LlmSection,
    #[serde(default = "default_tools_section")]
    #[allow(dead_code)]
    pub tools: ToolsSection,
    #[serde(default = "default_agent_section")]
    pub agent: AgentSection,
    #[serde(default = "default_notification_section")]
    pub notification: NotificationSection,
    #[serde(default)]
    pub tool: HashMap<String, toml::Value>,
    #[serde(default)]
    pub mcp: McpConfig,
    #[serde(default = "default_storage_section")]
    pub storage: StorageSection,
    #[serde(default)]
    #[allow(dead_code)]
    pub observability: ObservabilityConfig,
    /// 生命周期 hook 规则（设计 §7）。
    ///
    /// 宽松反序列化：整节解析失败时降级为空规则集（设计 §1.2，hook 问题不阻断启动）。
    #[serde(
        default,
        deserialize_with = "crate::hooks::deserialize_hooks_lenient",
        skip_serializing_if = "HooksConfig::is_empty"
    )]
    pub hooks: HooksConfig,
}

impl Default for DaemonConfig {
    fn default() -> Self {
        DaemonConfig {
            daemon: default_daemon_section(),
            llm: LlmSection::default(),
            tools: default_tools_section(),
            agent: default_agent_section(),
            notification: default_notification_section(),
            tool: HashMap::new(),
            mcp: McpConfig::default(),
            storage: StorageSection::default(),
            observability: ObservabilityConfig::default(),
            hooks: HooksConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonSection {
    #[serde(default = "default_listen_addr")]
    pub listen_addr: String,
    #[serde(default = "default_log_level")]
    pub log_level: String,
    /// 自动文件监听热重载开关（设计 §7 决策 12）。默认开启；关闭后退回纯显式
    /// `/reload` 形态。daemon.toml 不热重载，故该开关重启生效。
    #[serde(default = "default_filewatcher")]
    pub filewatcher: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct LlmSection {
    /// Claude thinking 模式预算 token 数（如 2048）
    #[serde(default)]
    pub thinking_budget_tokens: Option<u32>,
    /// 额外的 provider 特定参数（如 OpenAI 的 seed, response_format 等）
    #[serde(default)]
    pub extra: HashMap<String, String>,
    /// 多模型配置列表
    #[serde(default)]
    pub models: Vec<LlmModelConfig>,
    /// 默认模型 key（格式 {provider}/{name}），缺省时使用 models 第一个
    #[serde(default)]
    pub default: Option<String>,
    /// 默认文生图模型 key（格式 {provider}/{name}）
    #[serde(default)]
    pub image_generation_model: Option<String>,
    /// 默认识图模型 key（格式 {provider}/{name}）
    #[serde(default)]
    pub vision_model: Option<String>,
}

/// 单个 LLM 模型配置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmModelConfig {
    /// 模型显示名（在 /model 列表中展示）
    pub name: String,
    /// 驱动协议（openai / anthropic）
    pub protocol: String,
    /// 服务商名字，如 "Anthropic" / "OpenAI"；缺省时使用 protocol
    #[serde(default)]
    pub provider: Option<String>,
    /// 请求时的模型 key
    pub model: String,
    #[serde(default)]
    pub api_key: Option<String>,
    #[serde(default)]
    pub base_url: Option<String>,
    #[serde(default)]
    pub temperature: Option<f64>,
    #[serde(default)]
    pub max_tokens: Option<u32>,
    #[serde(default)]
    pub max_context_tokens: Option<u32>,
    #[serde(default)]
    pub thinking_budget_tokens: Option<u32>,
    /// 是否在请求中携带工具定义（false 时请求不带 tools）
    #[serde(default)]
    pub use_tool: Option<bool>,
    /// 标记该模型为文生图模型（使用 /images/generations 端点而非 /chat/completions）
    #[serde(default)]
    pub image_generation: Option<bool>,
    #[serde(default)]
    #[allow(dead_code)]
    pub extra: std::collections::HashMap<String, String>,
}

impl LlmModelConfig {
    /// 全局唯一标识 `{provider}/{name}`，作为模型切换的 lookup key
    pub fn key(&self) -> String {
        let p = self.provider.as_deref().unwrap_or(&self.protocol);
        format!("{p}/{}", self.name)
    }

    /// 模型别名 `{provider}/{model}`，作为备选 lookup key
    pub fn model_alias(&self) -> String {
        let p = self.provider.as_deref().unwrap_or(&self.protocol);
        format!("{p}/{}", self.model)
    }

    /// 检查给定的 key 是否匹配此模型（匹配 key 或 model_alias）
    pub fn matches_key(&self, candidate: &str) -> bool {
        self.key() == candidate || self.model_alias() == candidate
    }

    /// 判断两个模型配置是否为同一个模型（用于 config merge 时的匹配）
    fn matches_model(&self, other: &LlmModelConfig) -> bool {
        self.matches_key(&other.key())
            || self.matches_key(&other.model_alias())
            || (self.model == other.model && self.protocol == other.protocol)
    }

    /// 用 `override_cfg` 的值覆盖当前配置中已设置（Some）的字段。
    /// 用于 project config 覆盖 global config。
    fn merge_override(&mut self, override_cfg: &LlmModelConfig) {
        // 必填字段：override 总是覆盖
        self.name = override_cfg.name.clone();
        self.protocol = override_cfg.protocol.clone();
        self.model = override_cfg.model.clone();
        // Optional 字段：仅当 override 中为 Some 时覆盖
        if override_cfg.provider.is_some() {
            self.provider = override_cfg.provider.clone();
        }
        if override_cfg.api_key.is_some() {
            self.api_key = override_cfg.api_key.clone();
        }
        if override_cfg.base_url.is_some() {
            self.base_url = override_cfg.base_url.clone();
        }
        if override_cfg.temperature.is_some() {
            self.temperature = override_cfg.temperature;
        }
        if override_cfg.max_tokens.is_some() {
            self.max_tokens = override_cfg.max_tokens;
        }
        if override_cfg.max_context_tokens.is_some() {
            self.max_context_tokens = override_cfg.max_context_tokens;
        }
        if override_cfg.thinking_budget_tokens.is_some() {
            self.thinking_budget_tokens = override_cfg.thinking_budget_tokens;
        }
        // extra: merge map, override entries 优先
        for (k, v) in &override_cfg.extra {
            self.extra.insert(k.clone(), v.clone());
        }
    }
}

impl LlmSection {
    /// 返回有效的模型配置列表
    pub fn effective_models(&self) -> Vec<LlmModelConfig> {
        self.models.clone()
    }

    /// 返回可用的模型显示标签列表
    pub fn available_models(&self) -> Vec<String> {
        if self.models.is_empty() {
            return vec![];
        }
        self.models
            .iter()
            .map(|m| {
                let display_provider = m.provider.as_deref().unwrap_or(&m.protocol);
                format!("{}({})", m.name, display_provider)
            })
            .collect()
    }

    /// 解析默认模型 key（格式 {provider}/{name} 或 {provider}/{model}）。
    /// 优先使用 `default` 字段指定的模型，找不到时回退到 model_configs 第一个。
    pub fn resolve_default_key(&self, model_configs: &[LlmModelConfig]) -> String {
        if let Some(ref default_name) = self.default {
            if let Some(mc) = model_configs.iter().find(|mc| mc.matches_key(default_name)) {
                return mc.key();
            }
            tracing::warn!(
                default = %default_name,
                available = %model_configs.iter().map(|m| m.key()).collect::<Vec<_>>().join(", "),
                "llm.default points to unknown model, falling back to first model"
            );
        }
        model_configs
            .first()
            .map(|mc| mc.key())
            .unwrap_or_else(|| "default".to_string())
    }
}

/// LLM 配置参数
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LlmConfig {
    /// API model key（发送到 LLM API 的 model 字段，如 "deepseek-v4-flash"）
    pub model: String,
    /// Provider lookup key（格式 "{provider}/{name}"，如 "Opencode/DeepSeek v4 Flash"）。
    /// 用于从 providers HashMap 中查找正确的 provider 实例。
    /// None 时回退到 default_provider_key。
    pub model_key: Option<String>,
    /// Provider 名称（来自 daemon.toml [[llm.models]] provider 字段，缺省时为 protocol）。
    /// 用于 trace span 和 metrics summary 中记录 gen_ai.provider.name。
    pub provider: Option<String>,
    /// 温度（0.0-2.0）
    pub temperature: f64,
    /// 最大 token 数
    pub max_tokens: u32,
    /// 最大上下文 token 数（默认 128_000）
    pub max_context_tokens: u32,
    /// 扩展参数（provider 特定参数）
    pub extra: HashMap<String, String>,
    /// Langfuse 总开关（控制 gen_ai.client.operation span 上 trace 级字段记录）
    pub langfuse_enabled: bool,
    /// Langfuse session.id（若不设置则不记录）
    pub langfuse_session_id: Option<String>,
    /// Langfuse trace.name（若不设置则不记录）
    pub langfuse_trace_name: Option<String>,
    /// Langfuse user.id 字段值（None = 不设置）
    pub langfuse_user_id: Option<String>,
    /// Langfuse tags 字段值（JSON 字符串，None = 不设置）
    pub langfuse_tags: Option<String>,
    /// Langfuse environment 字段
    pub langfuse_environment: Option<String>,
    /// Langfuse release 字段
    pub langfuse_release: Option<String>,
    /// Langfuse version 字段
    pub langfuse_version: Option<String>,
    /// Langfuse public 开关（None = 不设置）
    pub langfuse_public: Option<bool>,
    /// Langfuse metadata（值均为字符串，记录到 span 时序列化为紧凑 JSON）
    pub langfuse_metadata: Option<HashMap<String, String>>,
    /// 是否在 Langfuse OTEL span 中记录 LLM generation input
    pub langfuse_capture_input: bool,
    /// 是否在 Langfuse OTEL span 中记录 LLM generation output
    pub langfuse_capture_output: bool,
    /// Langfuse 捕获的最大字符数（超出截断）
    pub langfuse_capture_max_chars: usize,
    /// 是否脱敏敏感字段（api_key/token/secret/password 等）
    pub langfuse_redact_secrets: bool,
    /// 是否在请求中携带工具定义（false 时请求不带 tools）
    pub use_tool: bool,
    /// 是否为文生图模型（true 时使用 /images/generations 端点）
    pub image_generation: bool,
    /// 会话 ID（运行时由 agent loop 注入，每会话稳定）。
    /// opencode 网关要求请求携带 x-opencode-session 头用于服务优化。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
}

impl Default for LlmConfig {
    fn default() -> Self {
        Self {
            model: "claude-3-7-sonnet-20250219".to_string(),
            model_key: None,
            provider: None,
            temperature: 0.7,
            max_tokens: 4096,
            max_context_tokens: 128_000,
            extra: HashMap::new(),
            langfuse_enabled: false,
            langfuse_session_id: None,
            langfuse_trace_name: None,
            langfuse_user_id: None,
            langfuse_tags: None,
            langfuse_environment: None,
            langfuse_release: None,
            langfuse_version: None,
            langfuse_public: None,
            langfuse_metadata: None,
            langfuse_capture_input: false,
            langfuse_capture_output: false,
            langfuse_capture_max_chars: 20_000,
            langfuse_redact_secrets: true,
            use_tool: true,
            image_generation: false,
            session_id: None,
        }
    }
}

/// 模型解析结果：当 agent 定义中指定了 `model` key（如 `"Opencode/deepseek-v4-flash"`）时，
/// orchestrator 通过此结构解析出实际的 API model 字符串、provider 名称等，
/// 覆盖从父会话继承的 `LlmConfig`。
#[derive(Debug, Clone)]
pub struct ModelInfo {
    /// 发送到 LLM API 的 model 字段（如 "deepseek-v4-flash"）
    pub model: String,
    /// Provider 名称（来自 daemon.toml [[llm.models]] provider 字段）
    pub provider: Option<String>,
    /// 默认温度
    pub temperature: Option<f64>,
    /// 默认 max_tokens
    pub max_tokens: Option<u32>,
    /// 默认 max_context_tokens
    pub max_context_tokens: Option<u32>,
    /// 是否为文生图模型（使用 /images/generations 端点）
    pub image_generation: bool,
    /// 是否在请求中携带工具定义
    pub use_tool: Option<bool>,
}

/// 将 project config 的 LLM 部分合并到 global config 中。
/// 优先级：project config > global config。
/// - project 中新增的模型会被追加到 models 列表。
/// - project 中已存在的模型（key/model_alias/model+protocol 匹配）会用 project 的值覆盖 global 的值。
/// - project 的 default 若设置，则覆盖 global 的 default。
fn merge_llm_sections(global: &mut LlmSection, project: &LlmSection) {
    if project.thinking_budget_tokens.is_some() {
        global.thinking_budget_tokens = project.thinking_budget_tokens;
    }
    if project.default.is_some() {
        global.default = project.default.clone();
    }
    if project.image_generation_model.is_some() {
        global.image_generation_model = project.image_generation_model.clone();
    }
    if project.vision_model.is_some() {
        global.vision_model = project.vision_model.clone();
    }
    // extra: merge map, project entries 优先
    for (k, v) in &project.extra {
        global.extra.insert(k.clone(), v.clone());
    }
    // models: 对 project 中的每个模型，尝试在 global 中找到匹配项并 merge override，
    // 如果找不到匹配项则追加为新模型
    for project_model in &project.models {
        let mut found = false;
        for global_model in &mut global.models {
            if global_model.matches_model(project_model) {
                global_model.merge_override(project_model);
                found = true;
                break;
            }
        }
        if !found {
            global.models.push(project_model.clone());
        }
    }
}

/// 将 project config 的 [[agent.builtin]] 部分合并到 global config 中。
/// 优先级：project config > global config。
/// - 按 name 匹配同名 agent，project 的 model/temperature/steps（仅当 Some）覆盖 global 的值。
/// - project 中新增的 agent（global 中无同名项）会被追加。
fn merge_agent_builtins(global: &mut Vec<BuiltinAgentConfig>, project: &[BuiltinAgentConfig]) {
    for project_item in project {
        let mut found = false;
        for global_item in &mut *global {
            if global_item.name == project_item.name {
                // 字段级合并：project 的 Some 覆盖 global，None 保留 global 值
                if let Some(ref model) = project_item.model {
                    global_item.model = Some(model.clone());
                }
                if let Some(temperature) = project_item.temperature {
                    global_item.temperature = Some(temperature);
                }
                if let Some(steps) = project_item.steps {
                    global_item.steps = Some(steps);
                }
                found = true;
                break;
            }
        }
        if !found {
            global.push(project_item.clone());
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolsSection {
    #[allow(dead_code)]
    #[serde(default = "default_bash_timeout")]
    pub bash_timeout_secs: u64,
    #[allow(dead_code)]
    #[serde(default = "default_file_max_size")]
    pub file_max_size_bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentSection {
    #[serde(default = "default_soft_limit", alias = "max_iterations")]
    pub soft_limit: u32,
    #[serde(default = "default_doom_loop_threshold")]
    pub doom_loop_threshold: u32,
    #[serde(default = "default_retry_attempts")]
    pub llm_retry_attempts: u32,
    #[serde(default = "default_retry_delay")]
    pub llm_retry_base_delay_ms: u64,
    #[serde(default = "default_bash_confirm")]
    pub bash_confirm_mode: bool,
    #[serde(default = "default_file_max_size")]
    pub file_max_size_bytes: u64,
    #[serde(default = "default_max_depth")]
    pub max_depth: u32,
    /// 全局同时运行的 subagent 数量上限（主 agent 不计入，超出排队等待）。
    /// 0 表示无限制（所有 subagent 立即并行执行，不排队）。
    #[serde(default = "default_max_concurrent_subagents")]
    pub max_concurrent_subagents: u32,
    /// 内置 agent 覆盖配置（model / temperature / steps）
    #[serde(default)]
    pub builtin: Vec<BuiltinAgentConfig>,
}

/// 内置 agent 配置覆盖项，用于在 daemon.toml 中为内置 agent
/// （如 explorer、fixer）指定 LLM 模型等参数。
///
/// ```toml
/// [[agent.builtin]]
/// name = "explorer"
/// model = "Opencode/deepseek-v4-flash"
/// temperature = 0.1
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BuiltinAgentConfig {
    /// 内置 agent 名称，如 "explorer"、"fixer"
    pub name: String,
    /// 模型 key（格式 {provider}/{model} 或 {provider}.{name}）
    #[serde(default)]
    pub model: Option<String>,
    /// 温度
    #[serde(default)]
    pub temperature: Option<f32>,
    /// 最大迭代次数
    #[serde(default)]
    pub steps: Option<u32>,
}

/// 终端通知协议
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NotificationProtocol {
    /// 自动探测终端支持的通知协议（默认值由字段级 default 提供）
    #[default]
    Auto,
    /// OSC 9（iTerm2 / Windows Terminal 等）
    Osc9,
    /// OSC 777（ConEmu 等）
    Osc777,
    /// Kitty 键盘协议通知（kitty 终端）
    Kitty99,
}

/// 终端通知配置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NotificationSection {
    /// 是否启用终端通知
    #[serde(default = "default_notification_enabled")]
    pub enabled: bool,
    /// 任务完成时是否通知
    #[serde(default = "default_notification_on_complete")]
    pub on_complete: bool,
    /// 等待用户输入时是否通知
    #[serde(default = "default_notification_on_user_input")]
    pub on_user_input: bool,
    /// 通知协议（auto / osc9 / osc777 / kitty99）
    #[serde(default = "default_notification_protocol")]
    pub protocol: NotificationProtocol,
    /// 同一会话内通知的最小间隔（秒），用于限流
    #[serde(default = "default_notification_min_interval_secs")]
    pub min_interval_secs: u64,
}

impl Default for NotificationSection {
    fn default() -> Self {
        Self {
            enabled: default_notification_enabled(),
            on_complete: default_notification_on_complete(),
            on_user_input: default_notification_on_user_input(),
            protocol: default_notification_protocol(),
            min_interval_secs: default_notification_min_interval_secs(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct StorageSection {
    #[serde(default = "default_storage_driver")]
    pub driver: String,
    #[serde(default = "default_storage_path")]
    pub path: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ObservabilityConfig {
    #[serde(default = "default_observability_enabled")]
    pub enabled: bool,
    #[serde(default = "default_observability_level")]
    pub level: String,
    #[serde(default = "default_observability_format")]
    pub format: String,
    #[serde(default = "default_observability_parent_link")]
    pub parent_link: bool,
    #[serde(default = "default_observability_metrics_summary")]
    pub metrics_summary: bool,
    #[serde(default = "default_observability_log_file")]
    pub log_file: Option<String>,
    #[serde(default)]
    pub otlp: OtlpConfig,
    #[serde(default)]
    pub langfuse: LangfuseConfig,
}

/// Langfuse capture switch configuration
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LangfuseCaptureConfig {
    #[serde(default)]
    pub input: bool,
    #[serde(default)]
    pub output: bool,
    /// 最大字符数（超出截断，默认 20000）
    #[serde(default = "default_capture_max_chars")]
    pub max_chars: usize,
    /// 是否脱敏敏感字段（默认 true）
    #[serde(default = "default_capture_redact_secrets")]
    pub redact_secrets: bool,
}

fn default_capture_max_chars() -> usize {
    20_000
}

fn default_capture_redact_secrets() -> bool {
    true
}

impl Default for LangfuseCaptureConfig {
    fn default() -> Self {
        Self {
            input: false,
            output: false,
            max_chars: default_capture_max_chars(),
            redact_secrets: default_capture_redact_secrets(),
        }
    }
}

/// Langfuse 观测配置
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct LangfuseConfig {
    /// 总开关（默认关闭，需显式开启）
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub user_id: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub environment: Option<String>,
    #[serde(default)]
    pub release: Option<String>,
    #[serde(default)]
    pub version: Option<String>,
    /// 三态：未设置/true/false（非布尔值报错）
    #[serde(default)]
    pub public: Option<bool>,
    /// 任意元数据（标量原样，数组/table 转紧凑 JSON）
    #[serde(default)]
    pub metadata: Option<toml::value::Table>,
    /// P1 capture 开关（仅解析，功能待实现）
    #[serde(default)]
    pub capture: LangfuseCaptureConfig,
}

impl Default for ObservabilityConfig {
    fn default() -> Self {
        Self {
            enabled: default_observability_enabled(),
            level: default_observability_level(),
            format: default_observability_format(),
            parent_link: default_observability_parent_link(),
            metrics_summary: default_observability_metrics_summary(),
            log_file: default_observability_log_file(),
            otlp: OtlpConfig::default(),
            langfuse: LangfuseConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct OtlpConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_otlp_endpoint")]
    pub endpoint: String,
    #[serde(default = "default_otlp_protocol")]
    pub protocol: String,
    #[serde(default = "default_otlp_timeout_secs")]
    pub timeout_secs: u64,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    #[serde(
        default = "default_sample_rate",
        deserialize_with = "deserialize_clamped_sample_rate"
    )]
    pub sample_rate: f64,
}

impl Default for OtlpConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            endpoint: default_otlp_endpoint(),
            protocol: default_otlp_protocol(),
            timeout_secs: default_otlp_timeout_secs(),
            headers: BTreeMap::new(),
            sample_rate: default_sample_rate(),
        }
    }
}

fn default_otlp_endpoint() -> String {
    "http://localhost:4317".into()
}
fn default_otlp_protocol() -> String {
    "grpc".into()
}
fn default_otlp_timeout_secs() -> u64 {
    10
}
fn default_sample_rate() -> f64 {
    1.0
}

fn deserialize_clamped_sample_rate<'de, D>(deserializer: D) -> Result<f64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = f64::deserialize(deserializer)?;
    Ok(value.clamp(0.0, 1.0))
}

fn default_listen_addr() -> String {
    "[::1]:50051".into()
}
fn default_log_level() -> String {
    "info".into()
}
fn default_filewatcher() -> bool {
    true
}
fn default_bash_timeout() -> u64 {
    120
}
fn default_file_max_size() -> u64 {
    1048576
}

fn default_max_depth() -> u32 {
    5
}
fn default_max_concurrent_subagents() -> u32 {
    3
}
fn default_soft_limit() -> u32 {
    50
}
fn default_doom_loop_threshold() -> u32 {
    5
}
fn default_retry_attempts() -> u32 {
    3
}
fn default_retry_delay() -> u64 {
    1000
}
fn default_bash_confirm() -> bool {
    true
}

fn default_storage_driver() -> String {
    "sqlite".into()
}

fn default_storage_path() -> String {
    "~/.visp/data/visp.db".into()
}

fn default_storage_section() -> StorageSection {
    StorageSection {
        driver: default_storage_driver(),
        path: default_storage_path(),
    }
}

fn default_observability_enabled() -> bool {
    true
}
fn default_observability_level() -> String {
    "info".into()
}
fn default_observability_format() -> String {
    "json".into()
}
fn default_observability_parent_link() -> bool {
    true
}
fn default_observability_metrics_summary() -> bool {
    true
}
fn default_observability_log_file() -> Option<String> {
    Some("~/.visp/logs".into())
}

/// `[hooks]` 规则级降级：剔除无效规则并逐条 `warn`，保留有效规则（设计 §1.2）。
///
/// hook 配置问题**不阻断 visp 启动**；问题暴露交由 `visp hooks doctor`（后续任务）。
/// 整节解析失败由 [`crate::hooks::deserialize_hooks_lenient`] 更早降级为空规则集。
fn degrade_invalid_hooks(hooks: &mut HooksConfig, source: &str) {
    for error in hooks.sanitize() {
        tracing::warn!(
            source = %source,
            error = %error,
            "invalid hook rule dropped; visp startup continues (run `visp hooks doctor` to inspect)"
        );
    }
}

pub fn load_config(config_path: Option<&Path>) -> Result<DaemonConfig, String> {
    // 1. 如果通过 CLI 参数指定了配置文件，直接使用该文件（最高优先级，跳过 merge）
    if let Some(path) = config_path {
        let mut config = load_from_file(path)?;
        degrade_invalid_hooks(&mut config.hooks, &path.display().to_string());
        return Ok(config);
    }

    // 2. 加载全局配置 ~/.config/visp/daemon.toml
    let mut config = if let Some(global_path) = path::daemon_toml_global() {
        if global_path.exists() {
            load_from_file(&global_path)?
        } else {
            default_config()
        }
    } else {
        default_config()
    };
    degrade_invalid_hooks(&mut config.hooks, "global config");

    // 3. 加载项目配置 cwd/.visp/daemon.toml，merge 到全局配置（项目优先级更高）
    //    合并范围：[llm] 全部字段 + [[agent.builtin]]（按 name 字段级合并）+ [hooks]（同 id 覆盖）
    let project_path = path::daemon_toml_project(&path::project_dir());
    if project_path.exists() {
        tracing::info!(
            path = %project_path.display(),
            "loading project-level config"
        );
        match load_from_file(&project_path) {
            Ok(mut project_config) => {
                degrade_invalid_hooks(
                    &mut project_config.hooks,
                    &project_path.display().to_string(),
                );
                merge_llm_sections(&mut config.llm, &project_config.llm);
                merge_agent_builtins(&mut config.agent.builtin, &project_config.agent.builtin);
                merge_hooks(&mut config.hooks, &project_config.hooks);
            }
            Err(e) => {
                tracing::warn!(path = %project_path.display(), error = %e, "failed to load project config, ignoring");
            }
        }
    }

    // 4. 环境变量覆盖（最高优先级，仅在对应的 env var 已设置时生效）
    //    VISP_LISTEN_ADDR -> daemon.listen_addr
    if let Ok(addr) = std::env::var("VISP_LISTEN_ADDR") {
        config.daemon.listen_addr = addr;
    }
    //    RUST_LOG -> daemon.log_level（日志过滤传统语义）
    if let Ok(level) = std::env::var("RUST_LOG") {
        config.daemon.log_level = level;
    }
    //    api_key = "${VAR}" -> 展开为环境变量 VAR（docs/daemon.example.toml 就是这样写的）；
    //    VAR 未设置或为空时视为未配置，交给下面的 OPENAI_API_KEY / ANTHROPIC_API_KEY 回填。
    for model in &mut config.llm.models {
        expand_api_key_env(model);
    }
    //    OPENAI_API_KEY -> 回填 openai 模型的 api_key（仅当该模型未配置 api_key）
    if let Ok(api_key) = std::env::var("OPENAI_API_KEY") {
        for model in &mut config.llm.models {
            if model.api_key.is_none() && model_matches_provider(model, "openai") {
                model.api_key = Some(api_key.clone());
            }
        }
    }
    //    ANTHROPIC_API_KEY -> 回填 anthropic 模型的 api_key（仅当该模型未配置 api_key）
    if let Ok(api_key) = std::env::var("ANTHROPIC_API_KEY") {
        for model in &mut config.llm.models {
            if model.api_key.is_none() && model_matches_provider(model, "anthropic") {
                model.api_key = Some(api_key.clone());
            }
        }
    }

    Ok(config)
}

/// 若 `value` 整体是 `${NAME}` 形式的环境变量引用，返回 `NAME`。
fn env_var_reference(value: &str) -> Option<&str> {
    let name = value.trim().strip_prefix("${")?.strip_suffix('}')?;
    let valid = !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    valid.then_some(name)
}

/// 展开 `api_key = "${NAME}"`。以前这串字面量会原样作为 key 发给 provider（401），
/// 且因为 api_key 非空，环境变量回填也不会生效。
fn expand_api_key_env(model: &mut LlmModelConfig) {
    let Some(name) = model.api_key.as_deref().and_then(env_var_reference) else {
        return;
    };
    let name = name.to_string();
    match std::env::var(&name) {
        Ok(value) if !value.is_empty() => model.api_key = Some(value),
        _ => {
            tracing::warn!(
                model = %model.name,
                var = %name,
                "api_key references an unset environment variable; treating it as not configured"
            );
            model.api_key = None;
        }
    }
}

/// 判断模型是否匹配给定的 provider 关键字：protocol 相等，或 provider 字段包含关键字（大小写不敏感）。
/// 用于环境变量（如 OPENAI_API_KEY）对模型 api_key 的回填匹配。
fn model_matches_provider(model: &LlmModelConfig, keyword: &str) -> bool {
    let protocol_matches = model.protocol.to_lowercase() == keyword;
    let provider_matches = model
        .provider
        .as_deref()
        .map(|p| p.to_lowercase().contains(keyword))
        .unwrap_or(false);
    protocol_matches || provider_matches
}

fn load_from_file(path: &Path) -> Result<DaemonConfig, String> {
    let content = std::fs::read_to_string(path).map_err(|e| format!("read config: {}", e))?;
    toml::from_str(&content).map_err(|e| format!("parse config: {}", e))
}

/// Initialize the global visp config directory tree and default config file.
///
/// If `~/.config/visp/daemon.toml` does not exist:
/// 1. Create `~/.config/visp/` directory
/// 2. Create `agents/`, `rules/`, `skills/` subdirectories
/// 3. Write default config to `~/.config/visp/daemon.toml`
///
/// If the config file already exists, this is a no-op.
/// Returns `Err` if `HOME` is not set.
pub fn init_config() -> Result<(), String> {
    let config_dir = path::global_config_dir()
        .ok_or_else(|| "HOME not set, cannot determine global config dir".to_string())?;
    let config_path = config_dir.join("daemon.toml");
    if config_path.exists() {
        return Ok(());
    }

    // Create directory tree
    std::fs::create_dir_all(&config_dir)
        .map_err(|e| format!("Failed to create config directory: {e}"))?;

    for sub in ["agents", "rules", "skills"] {
        std::fs::create_dir_all(config_dir.join(sub))
            .map_err(|e| format!("Failed to create {sub} directory: {e}"))?;
    }

    // Write default config
    let config = default_config();
    let content =
        toml::to_string_pretty(&config).map_err(|e| format!("Failed to serialize config: {e}"))?;

    let tmp = config_path.with_extension("toml.visp-tmp");
    std::fs::write(&tmp, &content).map_err(|e| format!("Failed to write temp file: {e}"))?;
    std::fs::rename(&tmp, &config_path).map_err(|e| format!("Failed to rename temp file: {e}"))?;

    tracing::info!(
        path = %config_path.display(),
        "created default global config"
    );

    Ok(())
}

/// 将配置原子写入 `{project}/.visp/daemon.toml`。
///
/// 全量序列化传入的 `DaemonConfig`，不做脱敏。
/// 写入策略：先写临时文件 `{target}.visp-tmp`，再 rename 到目标路径，
/// 保证写入过程中即使崩溃也不会产生损坏的配置文件。
pub fn save_config(config: &DaemonConfig, project: &Path) -> Result<(), String> {
    let target = path::daemon_toml_project(project);

    // 确保 .visp/ 目录存在
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("Failed to create .visp directory: {e}"))?;
    }

    // 序列化为 TOML
    let content =
        toml::to_string_pretty(config).map_err(|e| format!("Failed to serialize config: {e}"))?;

    // 原子写入：先写临时文件，再 rename
    let tmp = target.with_extension("toml.visp-tmp");
    std::fs::write(&tmp, &content).map_err(|e| format!("Failed to write temp file: {e}"))?;
    std::fs::rename(&tmp, &target).map_err(|e| format!("Failed to rename temp file: {e}"))?;

    Ok(())
}

fn default_daemon_section() -> DaemonSection {
    DaemonSection {
        listen_addr: default_listen_addr(),
        log_level: default_log_level(),
        filewatcher: default_filewatcher(),
    }
}

fn default_tools_section() -> ToolsSection {
    ToolsSection {
        bash_timeout_secs: default_bash_timeout(),
        file_max_size_bytes: default_file_max_size(),
    }
}

fn default_agent_section() -> AgentSection {
    AgentSection {
        soft_limit: default_soft_limit(),
        doom_loop_threshold: default_doom_loop_threshold(),
        llm_retry_attempts: default_retry_attempts(),
        llm_retry_base_delay_ms: default_retry_delay(),
        bash_confirm_mode: default_bash_confirm(),
        file_max_size_bytes: default_file_max_size(),
        max_depth: default_max_depth(),
        max_concurrent_subagents: default_max_concurrent_subagents(),
        builtin: Vec::new(),
    }
}

fn default_notification_enabled() -> bool {
    true
}
fn default_notification_on_complete() -> bool {
    true
}
fn default_notification_on_user_input() -> bool {
    true
}
fn default_notification_protocol() -> NotificationProtocol {
    NotificationProtocol::Auto
}
fn default_notification_min_interval_secs() -> u64 {
    3
}

fn default_notification_section() -> NotificationSection {
    NotificationSection {
        enabled: default_notification_enabled(),
        on_complete: default_notification_on_complete(),
        on_user_input: default_notification_on_user_input(),
        protocol: default_notification_protocol(),
        min_interval_secs: default_notification_min_interval_secs(),
    }
}

fn default_config() -> DaemonConfig {
    DaemonConfig {
        daemon: default_daemon_section(),
        llm: LlmSection {
            thinking_budget_tokens: None,
            extra: HashMap::new(),
            models: Vec::new(),
            default: None,
            image_generation_model: None,
            vision_model: None,
        },
        tools: default_tools_section(),
        agent: default_agent_section(),
        notification: default_notification_section(),
        tool: HashMap::new(),
        mcp: McpConfig::default(),
        storage: StorageSection {
            driver: default_storage_driver(),
            path: default_storage_path(),
        },
        observability: ObservabilityConfig::default(),
        hooks: HooksConfig::default(),
    }
}

// ============ Runtime config utilities ============

/// Convert proto LlmConfig to core LlmConfig.
/// Maps 6 proto fields, rest use LlmConfig::default().
pub fn proto_to_llm_config(proto: &visp_proto::visp::LlmConfig) -> LlmConfig {
    let mut config = LlmConfig::default();
    if let Some(model) = &proto.model {
        config.model = model.clone();
    }
    if let Some(model_key) = &proto.model_key {
        config.model_key = Some(model_key.clone());
    }
    if let Some(temperature) = proto.temperature {
        config.temperature = temperature;
    }
    if let Some(max_tokens) = proto.max_tokens {
        config.max_tokens = max_tokens;
    }
    if let Some(max_context_tokens) = proto.max_context_tokens {
        config.max_context_tokens = max_context_tokens;
    }
    config.extra = proto.extra.clone();
    config
}

/// Convert core LlmConfig to proto LlmConfig.
/// Maps 6 fields, drops 17 runtime-only fields.
pub fn llm_config_to_proto(config: &LlmConfig) -> visp_proto::visp::LlmConfig {
    visp_proto::visp::LlmConfig {
        model: Some(config.model.clone()),
        model_key: config.model_key.clone(),
        temperature: Some(config.temperature),
        max_tokens: Some(config.max_tokens),
        max_context_tokens: Some(config.max_context_tokens),
        extra: config.extra.clone(),
    }
}

/// Construct ModelInfo from LlmModelConfig.
pub fn model_config_to_info(mc: &LlmModelConfig) -> ModelInfo {
    ModelInfo {
        model: mc.model.clone(),
        provider: mc.provider.clone(),
        temperature: mc.temperature,
        max_tokens: mc.max_tokens,
        max_context_tokens: mc.max_context_tokens,
        image_generation: mc.image_generation.unwrap_or(false),
        use_tool: mc.use_tool,
    }
}

/// Resolve a model_key to its LlmModelConfig from DaemonConfig.llm.models.
pub fn resolve_model(model_key: &str, daemon_config: &DaemonConfig) -> Option<LlmModelConfig> {
    daemon_config
        .llm
        .models
        .iter()
        .find(|mc| mc.matches_key(model_key))
        .cloned()
}

/// Resolve the effective model_key using 4-tier cascade:
/// 1. agent_model (if Some and exists in providers)
/// 2. session_config.model_key (if Some and exists in providers)
/// 3. session_config.model (if exists as direct key - backward compat)
/// 4. daemon_config's default provider key
///
/// Note: This function checks against daemon_config.llm.models to see if a key exists.
/// The orchestrator will then use the returned key to look up the actual provider instance.
pub fn resolve_model_key(
    agent_model: Option<&str>,
    session_config: &LlmConfig,
    daemon_config: &DaemonConfig,
) -> String {
    let exists = |key: &str| {
        daemon_config
            .llm
            .models
            .iter()
            .any(|mc| mc.matches_key(key))
    };

    if let Some(key) = agent_model
        && exists(key)
    {
        return key.to_string();
    }
    if let Some(key) = &session_config.model_key
        && exists(key)
    {
        return key.clone();
    }
    if exists(&session_config.model) {
        return session_config.model.clone();
    }
    daemon_config
        .llm
        .resolve_default_key(&daemon_config.llm.models)
}

/// Apply model config overrides to an LlmConfig in-place.
/// Sets model, provider, temperature, max_tokens, max_context_tokens,
/// image_generation, use_tool from the LlmModelConfig.
/// Optional fields are only applied when set (Some).
pub fn apply_model_override(config: &mut LlmConfig, model_cfg: &LlmModelConfig) {
    config.model = model_cfg.model.clone();
    if let Some(provider) = &model_cfg.provider {
        config.provider = Some(provider.clone());
    }
    if let Some(temperature) = model_cfg.temperature {
        config.temperature = temperature;
    }
    if let Some(max_tokens) = model_cfg.max_tokens {
        config.max_tokens = max_tokens;
    }
    if let Some(max_context_tokens) = model_cfg.max_context_tokens {
        config.max_context_tokens = max_context_tokens;
    }
    if let Some(image_generation) = model_cfg.image_generation {
        config.image_generation = image_generation;
    }
    if let Some(use_tool) = model_cfg.use_tool {
        config.use_tool = use_tool;
    }
    // 合并模型级 extra（如 image_api = "dashscope"），模型配置优先
    for (k, v) in &model_cfg.extra {
        config.extra.insert(k.clone(), v.clone());
    }
}

// ============ Session LLM config merge utilities ============

/// 解析 daemon 默认模型配置（llm.default 指定的模型，缺省时回退到 models 第一个）。
fn daemon_default_model(daemon_config: &DaemonConfig) -> Option<LlmModelConfig> {
    let key = daemon_config
        .llm
        .resolve_default_key(&daemon_config.llm.models);
    daemon_config
        .llm
        .models
        .iter()
        .find(|mc| mc.key() == key)
        .cloned()
}

/// 计算 daemon 默认 extra（[llm.extra] + 全局 thinking_budget_tokens + 默认模型的 thinking_budget_tokens）。
fn daemon_default_extra(daemon_config: &DaemonConfig) -> HashMap<String, String> {
    let mut extra = daemon_config.llm.extra.clone();
    if let Some(budget) = daemon_config.llm.thinking_budget_tokens {
        extra.insert("thinking_budget_tokens".into(), budget.to_string());
    }
    if let Some(mc) = daemon_default_model(daemon_config)
        && let Some(budget) = mc.thinking_budget_tokens
    {
        extra.insert("thinking_budget_tokens".into(), budget.to_string());
    }
    extra
}

/// Merge client-provided proto LlmConfig with daemon defaults to produce the final session LlmConfig.
///
/// Priority: client config > model_key resolution > daemon defaults
///
/// 1. Convert client proto LlmConfig to core LlmConfig (via proto_to_llm_config)
/// 2. If extra is empty, fill from daemon default model's extra
/// 3. If model_key is set, resolve from daemon_config.llm.models:
///    - Set model, provider from matched LlmModelConfig
///    - Only fill max_tokens, max_context_tokens, temperature if client didn't set them
///      (detect "unset" by comparing to LlmConfig::default() values, use f64::EPSILON for temperature)
///    - Inject thinking_budget_tokens into extra if present
/// 4. If model is still the default, fill from daemon default model config
/// 5. If model_key is still None, set from daemon default
/// 6. If provider is still None, set from daemon default
pub fn merge_session_config(
    client_config: Option<&visp_proto::visp::LlmConfig>,
    daemon_config: &DaemonConfig,
) -> LlmConfig {
    // 1. 客户端 proto → core LlmConfig
    let mut config = client_config.map(proto_to_llm_config).unwrap_or_default();

    // 2. extra 为空时用 daemon 默认 extra 填充
    if config.extra.is_empty() {
        config.extra = daemon_default_extra(daemon_config);
    }

    // 3. model_key 解析：匹配 daemon.toml 中的模型配置
    if let Some(model_key) = &config.model_key
        && let Some(mc) = daemon_config
            .llm
            .models
            .iter()
            .find(|mc| mc.matches_key(model_key))
    {
        config.model = mc.model.clone();
        config.provider = Some(mc.provider.clone().unwrap_or_else(|| mc.protocol.clone()));
        // 仅在客户端未显式设置时填充（与 LlmConfig::default() 哨兵值比较）
        if config.max_tokens == LlmConfig::default().max_tokens
            && let Some(mt) = mc.max_tokens
        {
            config.max_tokens = mt;
        }
        if config.max_context_tokens == LlmConfig::default().max_context_tokens
            && let Some(mct) = mc.max_context_tokens
        {
            config.max_context_tokens = mct;
        }
        if (config.temperature - LlmConfig::default().temperature).abs() < f64::EPSILON
            && let Some(t) = mc.temperature
        {
            config.temperature = t;
        }
        // per-model thinking_budget_tokens 注入 extra
        if let Some(budget) = mc.thinking_budget_tokens {
            config
                .extra
                .insert("thinking_budget_tokens".into(), budget.to_string());
        }
    }

    // 4-6. model 仍为默认值时，用 daemon 默认模型填充 model/model_key/provider
    if config.model == LlmConfig::default().model
        && let Some(mc) = daemon_default_model(daemon_config)
    {
        config.model = mc.model.clone();
        if config.model_key.is_none() {
            config.model_key = Some(mc.key());
        }
        if config.provider.is_none() {
            config.provider = Some(mc.provider.clone().unwrap_or_else(|| mc.protocol.clone()));
        }
    }
    config
}

/// Build a complete LlmConfig from a LlmModelConfig, optionally with langfuse settings.
///
/// `langfuse_cfg` provides langfuse-related fields (enabled, session_id, trace_name, etc.)
/// These fields are not in LlmModelConfig but come from ObservabilityConfig.langfuse_*.
/// Pass None to use disabled defaults for all langfuse fields.
pub fn build_llm_config_from_model(
    model_cfg: &LlmModelConfig,
    langfuse_cfg: Option<&ObservabilityConfig>,
) -> LlmConfig {
    let mut extra = HashMap::new();
    if let Some(budget) = model_cfg.thinking_budget_tokens {
        extra.insert("thinking_budget_tokens".into(), budget.to_string());
    }

    let mut config = LlmConfig {
        model: model_cfg.model.clone(),
        model_key: Some(model_cfg.key()),
        provider: Some(
            model_cfg
                .provider
                .clone()
                .unwrap_or_else(|| model_cfg.protocol.clone()),
        ),
        temperature: model_cfg.temperature.unwrap_or(0.7),
        max_tokens: model_cfg.max_tokens.unwrap_or(4096),
        max_context_tokens: model_cfg.max_context_tokens.unwrap_or(128_000),
        extra,
        ..LlmConfig::default()
    };

    if let Some(obs) = langfuse_cfg {
        let langfuse = &obs.langfuse;
        config.langfuse_enabled = langfuse.enabled;
        config.langfuse_user_id = langfuse.user_id.clone();
        config.langfuse_tags = if langfuse.tags.is_empty() {
            None
        } else {
            Some(serde_json::to_string(&langfuse.tags).unwrap_or_default())
        };
        config.langfuse_environment = langfuse.environment.clone();
        config.langfuse_release = langfuse.release.clone();
        config.langfuse_version = langfuse.version.clone();
        config.langfuse_public = langfuse.public;
        config.langfuse_metadata = langfuse.metadata.as_ref().map(|meta| {
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
        config.langfuse_capture_input = langfuse.capture.input;
        config.langfuse_capture_output = langfuse.capture.output;
        config.langfuse_capture_max_chars = langfuse.capture.max_chars;
        config.langfuse_redact_secrets = langfuse.capture.redact_secrets;
        // session_id / trace_name 为 per-session 字段，此处保持 None
    }

    config
}

/// Apply a config update (from /model, /temp commands) to the current session config.
///
/// Takes the current LlmConfig, a proto LlmConfig update, and daemon config.
/// Returns the updated LlmConfig.
///
/// Logic:
/// 1. Convert proto update to core LlmConfig fields (only the 6 proto fields)
/// 2. If update has model_key, resolve from daemon_config.llm.models:
///    - Set model, provider from matched LlmModelConfig
///    - Fill max_tokens, max_context_tokens, temperature using sentinel detection
///    - Inject thinking_budget_tokens into extra if present
/// 3. If update doesn't have model_key but has model, just update the model string
/// 4. If update has temperature (not default), update it
/// 5. If update has max_tokens (not default), update it
/// 6. Preserve all other fields from current config (langfuse, use_tool, etc.)
pub fn apply_config_update(
    current: &LlmConfig,
    update: &visp_proto::visp::LlmConfig,
    daemon_config: &DaemonConfig,
) -> LlmConfig {
    // 1. 从 current 出发，应用 proto 的 6 个字段
    let mut config = current.clone();

    // 2. model_key 优先：匹配 daemon.toml 中 [[llm.models]] 的配置
    if let Some(model_key) = &update.model_key {
        config.model_key = Some(model_key.clone());
        if let Some(mc) = daemon_config
            .llm
            .models
            .iter()
            .find(|mc| mc.matches_key(model_key))
        {
            config.model = mc.model.clone();
            config.provider = Some(mc.provider.clone().unwrap_or_else(|| mc.protocol.clone()));
            // 哨兵值检测：仅当 current 未设置非默认值时用模型配置填充
            if config.max_tokens == LlmConfig::default().max_tokens
                && let Some(mt) = mc.max_tokens
            {
                config.max_tokens = mt;
            }
            if config.max_context_tokens == LlmConfig::default().max_context_tokens
                && let Some(mct) = mc.max_context_tokens
            {
                config.max_context_tokens = mct;
            }
            if (config.temperature - LlmConfig::default().temperature).abs() < f64::EPSILON
                && let Some(t) = mc.temperature
            {
                config.temperature = t;
            }
            // per-model thinking_budget_tokens 注入 extra
            if let Some(budget) = mc.thinking_budget_tokens {
                config
                    .extra
                    .insert("thinking_budget_tokens".into(), budget.to_string());
            }
        }
    } else if let Some(model) = &update.model {
        // 3. 未传 model_key 但传了 model → 仅更新 model 字符串
        config.model = model.clone();
    }

    // 4-5. 显式设置的 temperature / max_tokens / max_context_tokens 覆盖
    if let Some(temperature) = update.temperature {
        config.temperature = temperature;
    }
    if let Some(max_tokens) = update.max_tokens {
        config.max_tokens = max_tokens;
    }
    if let Some(max_context_tokens) = update.max_context_tokens {
        config.max_context_tokens = max_context_tokens;
    }
    // extra：仅当 update 携带非空 extra 时替换，否则保留 current
    if !update.extra.is_empty() {
        config.extra = update.extra.clone();
    }

    // 6. 其余字段（langfuse、use_tool 等）从 current 保留
    config
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;
    use std::io::Write;
    use std::path::PathBuf;

    #[test]
    #[serial]
    fn test_init_config_creates_dir_tree_and_default_config() {
        let tmp = tempfile::tempdir().unwrap();

        // Save and override HOME
        let original_home = std::env::var("HOME").ok();
        unsafe { std::env::set_var("HOME", tmp.path()) };

        init_config().unwrap();

        let config_dir = tmp.path().join(".config").join("visp");

        // daemon.toml exists
        let config_path = config_dir.join("daemon.toml");
        assert!(config_path.exists(), "daemon.toml should exist");

        // Subdirectories exist
        for sub in ["agents", "rules", "skills"] {
            assert!(
                config_dir.join(sub).is_dir(),
                "{sub} directory should exist"
            );
        }

        // Config is valid and matches default
        let loaded: DaemonConfig = toml::from_str(&std::fs::read_to_string(&config_path).unwrap())
            .expect("written config should parse");
        assert_eq!(loaded.llm.default, default_config().llm.default);

        // Restore HOME
        match original_home {
            Some(h) => unsafe { std::env::set_var("HOME", h) },
            None => unsafe { std::env::remove_var("HOME") },
        }
    }

    #[test]
    #[serial]
    fn test_init_config_is_noop_when_config_exists() {
        let tmp = tempfile::tempdir().unwrap();

        let original_home = std::env::var("HOME").ok();
        unsafe { std::env::set_var("HOME", tmp.path()) };

        init_config().unwrap();
        let config_path = tmp.path().join(".config").join("visp").join("daemon.toml");
        let original = std::fs::read_to_string(&config_path).unwrap();

        // Call again: should be a no-op, content unchanged
        init_config().unwrap();
        let after = std::fs::read_to_string(&config_path).unwrap();
        assert_eq!(original, after);

        match original_home {
            Some(h) => unsafe { std::env::set_var("HOME", h) },
            None => unsafe { std::env::remove_var("HOME") },
        }
    }

    #[test]
    fn test_agent_max_concurrent_subagents_from_toml() {
        let toml_str = r#"
[agent]
max_concurrent_subagents = 5
"#;
        let config: DaemonConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.agent.max_concurrent_subagents, 5);
    }

    #[test]
    fn test_agent_max_concurrent_subagents_default() {
        // 缺省：无该字段时默认 3
        let config: DaemonConfig = toml::from_str("").unwrap();
        assert_eq!(config.agent.max_concurrent_subagents, 3);
        assert_eq!(default_agent_section().max_concurrent_subagents, 3);
    }

    #[test]
    fn test_merge_llm_sections_project_overrides_global_api_key_and_base_url() {
        let global_toml = r#"
[daemon]
listen_addr = "[::1]:50051"

[llm]
[[llm.models]]
name = "Sonnet"
protocol = "anthropic"
model = "claude-sonnet-4-20250514"
api_key = "global-key"
base_url = "https://global.example.com"

[tools]

[agent]
"#;
        let project_toml = r#"
[llm]
[[llm.models]]
name = "Sonnet"
protocol = "anthropic"
model = "claude-sonnet-4-20250514"
api_key = "project-key"
base_url = "https://project.example.com"
"#;
        let mut global: DaemonConfig = toml::from_str(global_toml).unwrap();
        let project: DaemonConfig = toml::from_str(project_toml).unwrap();

        merge_llm_sections(&mut global.llm, &project.llm);

        assert_eq!(global.llm.models.len(), 1);
        assert_eq!(global.llm.models[0].api_key.as_deref(), Some("project-key"));
        assert_eq!(
            global.llm.models[0].base_url.as_deref(),
            Some("https://project.example.com")
        );
    }

    #[test]
    fn test_merge_llm_sections_project_adds_new_model() {
        let global_toml = r#"
[daemon]
listen_addr = "[::1]:50051"

[llm]
[[llm.models]]
name = "Sonnet"
protocol = "anthropic"
model = "claude-sonnet-4-20250514"

[tools]

[agent]
"#;
        let project_toml = r#"
[llm]
[[llm.models]]
name = "GPT-4o"
protocol = "openai"
model = "gpt-4o"
api_key = "sk-project-key"
"#;
        let mut global: DaemonConfig = toml::from_str(global_toml).unwrap();
        let project: DaemonConfig = toml::from_str(project_toml).unwrap();

        merge_llm_sections(&mut global.llm, &project.llm);

        assert_eq!(global.llm.models.len(), 2);
        assert_eq!(global.llm.models[0].name, "Sonnet");
        assert_eq!(global.llm.models[1].name, "GPT-4o");
        assert_eq!(
            global.llm.models[1].api_key.as_deref(),
            Some("sk-project-key")
        );
    }

    #[test]
    fn test_merge_llm_sections_project_partial_override_keeps_global_fields() {
        let global_toml = r#"
[daemon]
listen_addr = "[::1]:50051"

[llm]
[[llm.models]]
name = "Sonnet"
protocol = "anthropic"
model = "claude-sonnet-4-20250514"
api_key = "global-key"
base_url = "https://global.example.com"
temperature = 0.5
max_tokens = 8192

[tools]

[agent]
"#;
        // project config only overrides api_key, leaves everything else
        let project_toml = r#"
[llm]
[[llm.models]]
name = "Sonnet"
protocol = "anthropic"
model = "claude-sonnet-4-20250514"
api_key = "project-key"
"#;
        let mut global: DaemonConfig = toml::from_str(global_toml).unwrap();
        let project: DaemonConfig = toml::from_str(project_toml).unwrap();

        merge_llm_sections(&mut global.llm, &project.llm);

        assert_eq!(global.llm.models.len(), 1);
        assert_eq!(global.llm.models[0].api_key.as_deref(), Some("project-key"));
        // base_url should be kept from global
        assert_eq!(
            global.llm.models[0].base_url.as_deref(),
            Some("https://global.example.com")
        );
        // temperature should be kept from global
        assert!((global.llm.models[0].temperature.unwrap() - 0.5).abs() < f64::EPSILON);
        // max_tokens should be kept from global
        assert_eq!(global.llm.models[0].max_tokens, Some(8192));
    }

    #[test]
    fn test_merge_llm_sections_project_overrides_default() {
        let global_toml = r#"
[daemon]
listen_addr = "[::1]:50051"

[llm]
default = "Anthropic/Sonnet"
[[llm.models]]
name = "Sonnet"
provider = "Anthropic"
protocol = "anthropic"
model = "claude-sonnet-4-20250514"

[[llm.models]]
name = "GPT-4o"
provider = "OpenAI"
protocol = "openai"
model = "gpt-4o"

[tools]

[agent]
"#;
        let project_toml = r#"
[llm]
default = "OpenAI/GPT-4o"
"#;
        let mut global: DaemonConfig = toml::from_str(global_toml).unwrap();
        let project: DaemonConfig = toml::from_str(project_toml).unwrap();

        merge_llm_sections(&mut global.llm, &project.llm);

        assert_eq!(global.llm.default.as_deref(), Some("OpenAI/GPT-4o"));
    }

    #[test]
    fn test_merge_llm_sections_match_by_model_alias() {
        // global config uses provider, project config omits provider
        // but model + protocol should still match
        let global_toml = r#"
[daemon]
listen_addr = "[::1]:50051"

[llm]
[[llm.models]]
name = "Sonnet"
provider = "Anthropic"
protocol = "anthropic"
model = "claude-sonnet-4-20250514"
api_key = "global-key"

[tools]

[agent]
"#;
        // project: no provider, but model + protocol match
        let project_toml = r#"
[llm]
[[llm.models]]
name = "Sonnet"
protocol = "anthropic"
model = "claude-sonnet-4-20250514"
api_key = "project-key"
"#;
        let mut global: DaemonConfig = toml::from_str(global_toml).unwrap();
        let project: DaemonConfig = toml::from_str(project_toml).unwrap();

        merge_llm_sections(&mut global.llm, &project.llm);

        // Should merge into one model, not add a second
        assert_eq!(global.llm.models.len(), 1);
        assert_eq!(global.llm.models[0].api_key.as_deref(), Some("project-key"));
    }

    #[test]
    fn test_merge_agent_builtins_project_adds_new_agent() {
        let mut global = vec![BuiltinAgentConfig {
            name: "explorer".into(),
            model: Some("opencode/deepseek-v4-flash".into()),
            temperature: Some(0.1),
            steps: None,
        }];
        let project = vec![BuiltinAgentConfig {
            name: "oracle".into(),
            model: Some("opencode/deepseek-v4-pro".into()),
            temperature: None,
            steps: Some(50),
        }];

        merge_agent_builtins(&mut global, &project);

        assert_eq!(global.len(), 2);
        assert_eq!(global[0].name, "explorer");
        assert_eq!(global[1].name, "oracle");
        assert_eq!(global[1].model.as_deref(), Some("opencode/deepseek-v4-pro"));
        assert_eq!(global[1].steps, Some(50));
    }

    #[test]
    fn test_merge_agent_builtins_project_overrides_existing_fields() {
        let mut global = vec![BuiltinAgentConfig {
            name: "explorer".into(),
            model: Some("opencode/deepseek-v4-flash".into()),
            temperature: Some(0.1),
            steps: Some(30),
        }];
        // project overrides model and temperature, but steps is None (keep global)
        let project = vec![BuiltinAgentConfig {
            name: "explorer".into(),
            model: Some("opencode/deepseek-v4-pro".into()),
            temperature: Some(0.5),
            steps: None,
        }];

        merge_agent_builtins(&mut global, &project);

        assert_eq!(global.len(), 1);
        assert_eq!(global[0].name, "explorer");
        // overridden by project
        assert_eq!(global[0].model.as_deref(), Some("opencode/deepseek-v4-pro"));
        assert_eq!(global[0].temperature, Some(0.5));
        // kept from global because project's steps is None
        assert_eq!(global[0].steps, Some(30));
    }

    #[test]
    fn test_merge_agent_builtins_empty_project_keeps_global() {
        let mut global = vec![
            BuiltinAgentConfig {
                name: "explorer".into(),
                model: Some("opencode/deepseek-v4-flash".into()),
                temperature: Some(0.1),
                steps: None,
            },
            BuiltinAgentConfig {
                name: "fixer".into(),
                model: None,
                temperature: None,
                steps: Some(30),
            },
        ];
        let project: Vec<BuiltinAgentConfig> = vec![];

        merge_agent_builtins(&mut global, &project);

        assert_eq!(global.len(), 2);
        assert_eq!(global[0].name, "explorer");
        assert_eq!(
            global[0].model.as_deref(),
            Some("opencode/deepseek-v4-flash")
        );
        assert_eq!(global[1].name, "fixer");
        assert_eq!(global[1].steps, Some(30));
    }

    #[test]

    fn test_llm_section_with_api_key() {
        let toml = r#"
[daemon]
listen_addr = "[::1]:50051"

[llm]
[[llm.models]]
name = "gpt-4"
protocol = "openai"
model = "gpt-4"
api_key = "sk-test-key"
base_url = "https://custom.api.com"

[tools]

[agent]
"#;
        let config: DaemonConfig = toml::from_str(toml).unwrap();
        assert_eq!(config.llm.models[0].api_key.as_deref(), Some("sk-test-key"));
        assert_eq!(
            config.llm.models[0].base_url.as_deref(),
            Some("https://custom.api.com")
        );
    }

    #[test]
    fn test_load_config_from_file() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        write!(
            file,
            r#"
[daemon]
listen_addr = "127.0.0.1:9090"
log_level = "debug"

[llm]
[[llm.models]]
name = "gpt-4"
protocol = "openai"
model = "gpt-4"
temperature = 0.5
max_tokens = 2048

[tools]
bash_timeout_secs = 60
file_max_size_bytes = 512000

[agent]
soft_limit = 10
llm_retry_attempts = 5
llm_retry_base_delay_ms = 500
bash_confirm_mode = false
file_max_size_bytes = 256000
"#,
        )
        .unwrap();

        let config = load_config(Some(file.path())).unwrap();
        assert_eq!(config.daemon.listen_addr, "127.0.0.1:9090");
        assert_eq!(config.daemon.log_level, "debug");
        assert_eq!(config.llm.models[0].protocol, "openai");
        assert_eq!(config.llm.models[0].model, "gpt-4");
        assert!((config.llm.models[0].temperature.unwrap() - 0.5).abs() < f64::EPSILON);
        assert_eq!(config.llm.models[0].max_tokens.unwrap(), 2048);
        assert_eq!(config.tools.bash_timeout_secs, 60);
        assert_eq!(config.tools.file_max_size_bytes, 512000);
        assert_eq!(config.agent.soft_limit, 10);
        assert_eq!(config.agent.llm_retry_attempts, 5);
        assert_eq!(config.agent.llm_retry_base_delay_ms, 500);
        assert!(!config.agent.bash_confirm_mode);
        assert_eq!(config.agent.file_max_size_bytes, 256000);
    }

    #[test]
    fn test_default_config_values() {
        let config = default_config();
        assert_eq!(config.daemon.listen_addr, "[::1]:50051");
        assert_eq!(config.daemon.log_level, "info");
        assert_eq!(config.llm.models.len(), 0);
        assert!(config.llm.models.is_empty());
        assert_eq!(config.tools.bash_timeout_secs, 120);
        assert_eq!(config.tools.file_max_size_bytes, 1048576);
        assert_eq!(config.agent.soft_limit, 50);
        assert_eq!(config.agent.llm_retry_attempts, 3);
        assert_eq!(config.agent.llm_retry_base_delay_ms, 1000);
        assert!(config.agent.bash_confirm_mode);
        assert_eq!(config.agent.file_max_size_bytes, 1048576);
    }

    #[test]
    fn test_config_with_explicit_max_context_tokens() {
        let toml = r#"
[daemon]
listen_addr = "[::1]:50051"

[llm]
[[llm.models]]
name = "test"
protocol = "anthropic"
model = "claude-3-7-sonnet-20250219"
max_context_tokens = 64000

[tools]

[agent]
"#;
        let config: DaemonConfig = toml::from_str(toml).unwrap();
        assert_eq!(config.llm.models[0].max_context_tokens, Some(64_000));
    }

    #[test]
    fn test_llmmodelconfig_with_thinking_budget_tokens() {
        let toml = r#"
[daemon]
listen_addr = "[::1]:50051"

[llm]
[[llm.models]]
name = "test"
protocol = "anthropic"
model = "claude-3-7-sonnet-20250219"
thinking_budget_tokens = 2048

[tools]

[agent]
"#;
        let config: DaemonConfig = toml::from_str(toml).unwrap();
        assert_eq!(config.llm.models[0].thinking_budget_tokens, Some(2048));
    }

    #[test]
    fn test_llmmodelconfig_without_thinking_budget_defaults_to_none() {
        let toml = r#"
[daemon]
listen_addr = "[::1]:50051"

[llm]
[[llm.models]]
name = "test"
protocol = "anthropic"
model = "claude-3-7-sonnet-20250219"

[tools]

[agent]
"#;
        let config: DaemonConfig = toml::from_str(toml).unwrap();
        assert_eq!(config.llm.models[0].thinking_budget_tokens, None);
    }

    #[test]
    fn test_config_missing_max_context_tokens_defaults() {
        let toml = r#"
[daemon]
listen_addr = "[::1]:50051"

[llm]
[[llm.models]]
name = "test"
protocol = "anthropic"
model = "claude-3-7-sonnet-20250219"

[tools]

[agent]
"#;
        let config: DaemonConfig = toml::from_str(toml).unwrap();
        assert_eq!(config.llm.models[0].max_context_tokens, None);
    }

    #[test]
    #[serial]
    fn test_load_config_auto_find() {
        use std::fs;

        // 创建一个临时 HOME 目录，里面放默认配置文件
        let tmp = tempfile::tempdir().unwrap();
        let config_dir = tmp.path().join(".config").join("visp");
        fs::create_dir_all(&config_dir).unwrap();
        let config_file = config_dir.join("daemon.toml");
        fs::write(
            &config_file,
            r#"
[daemon]
listen_addr = "0.0.0.0:8080"
log_level = "warn"

[llm]
[[llm.models]]
name = "DeepSeek"
protocol = "ollama"
model = "deepseek-v4-flash"
temperature = 0.1
max_tokens = 2048
api_key = "test-key"
base_url = "http://localhost:11434"

[tools]
bash_timeout_secs = 30
file_max_size_bytes = 512000

[agent]
soft_limit = 5
llm_retry_attempts = 1
llm_retry_base_delay_ms = 100
bash_confirm_mode = false
file_max_size_bytes = 256000
"#,
        )
        .unwrap();

        // 设置 HOME 指向临时目录
        let original_home = std::env::var("HOME").ok();
        // 清理环境变量覆盖项，避免外部环境（如 CI）干扰测试结果
        const OVERRIDE_VARS: [&str; 4] = [
            "VISP_LISTEN_ADDR",
            "RUST_LOG",
            "OPENAI_API_KEY",
            "ANTHROPIC_API_KEY",
        ];
        let original_overrides: Vec<(String, Option<String>)> = OVERRIDE_VARS
            .iter()
            .map(|k| (k.to_string(), std::env::var(k).ok()))
            .collect();
        // safety: 测试中单线程执行，env var 操作安全
        unsafe { std::env::set_var("HOME", tmp.path()) };
        for (key, _) in &original_overrides {
            unsafe { std::env::remove_var(key) };
        }

        let result = load_config(None);

        // 恢复 HOME
        // safety: 同上
        if let Some(home) = original_home {
            unsafe { std::env::set_var("HOME", &home) };
        } else {
            unsafe { std::env::remove_var("HOME") };
        }
        for (key, value) in &original_overrides {
            match value {
                Some(v) => unsafe { std::env::set_var(key, v) },
                None => unsafe { std::env::remove_var(key) },
            }
        }

        let config = result.unwrap();
        assert_eq!(config.daemon.listen_addr, "0.0.0.0:8080");
        assert_eq!(config.daemon.log_level, "warn");
        assert_eq!(config.llm.models[0].protocol, "ollama");
        assert_eq!(config.llm.models[0].model, "deepseek-v4-flash");
        assert_eq!(config.llm.models[0].api_key.as_deref(), Some("test-key"));
        assert_eq!(
            config.llm.models[0].base_url.as_deref(),
            Some("http://localhost:11434")
        );
        assert_eq!(config.tools.bash_timeout_secs, 30);
        assert_eq!(config.agent.soft_limit, 5);
    }

    /// Helper: 在临时 HOME 下写入 global 配置并执行 `load_config(None)`。
    /// 测试期间设置/清理给定的 env vars，结束后恢复原值。
    fn run_load_config_with_env(
        global_toml: &str,
        envs: &[(&str, Option<&str>)],
    ) -> Result<DaemonConfig, String> {
        use std::fs;

        let tmp = tempfile::tempdir().unwrap();
        let config_dir = tmp.path().join(".config").join("visp");
        fs::create_dir_all(&config_dir).unwrap();
        fs::write(config_dir.join("daemon.toml"), global_toml).unwrap();

        let original_home = std::env::var("HOME").ok();
        // 记录并清理全部环境变量覆盖项，再设置本次测试的 env vars
        const OVERRIDE_VARS: [&str; 4] = [
            "VISP_LISTEN_ADDR",
            "RUST_LOG",
            "OPENAI_API_KEY",
            "ANTHROPIC_API_KEY",
        ];
        let originals: Vec<(String, Option<String>)> = OVERRIDE_VARS
            .iter()
            .map(|k| (k.to_string(), std::env::var(k).ok()))
            .collect();
        // safety: 测试中单线程执行，env var 操作安全
        unsafe { std::env::set_var("HOME", tmp.path()) };
        for (key, _) in &originals {
            unsafe { std::env::remove_var(key) };
        }
        for (key, value) in envs {
            match value {
                Some(v) => unsafe { std::env::set_var(key, v) },
                None => unsafe { std::env::remove_var(key) },
            }
        }

        let result = load_config(None);

        // 恢复 env vars 与 HOME
        // safety: 同上
        for (key, value) in originals {
            match value {
                Some(v) => unsafe { std::env::set_var(&key, v) },
                None => unsafe { std::env::remove_var(&key) },
            }
        }
        if let Some(home) = original_home {
            unsafe { std::env::set_var("HOME", &home) };
        } else {
            unsafe { std::env::remove_var("HOME") };
        }
        result
    }

    #[test]
    #[serial]
    fn test_env_override_listen_addr() {
        let global_toml = r#"
[daemon]
listen_addr = "[::1]:50051"

[llm]

[tools]

[agent]
"#;
        let config =
            run_load_config_with_env(global_toml, &[("VISP_LISTEN_ADDR", Some("0.0.0.0:9999"))])
                .unwrap();
        assert_eq!(config.daemon.listen_addr, "0.0.0.0:9999");
    }

    #[test]
    #[serial]
    fn test_env_override_rust_log() {
        let global_toml = r#"
[daemon]
listen_addr = "[::1]:50051"

[llm]

[tools]

[agent]
"#;
        let config = run_load_config_with_env(global_toml, &[("RUST_LOG", Some("debug"))]).unwrap();
        // RUST_LOG -> daemon.log_level（日志过滤传统语义）
        assert_eq!(config.daemon.log_level, "debug");
        // RUST_LOG 不覆盖 observability.level（保持默认 info）
        assert_eq!(config.observability.level, "info");
    }

    #[test]
    #[serial]
    fn test_env_openai_api_key_backfill() {
        let global_toml = r#"
[daemon]
listen_addr = "[::1]:50051"

[llm]
[[llm.models]]
name = "GPT-4o"
protocol = "openai"
model = "gpt-4o"

[[llm.models]]
name = "Sonnet"
protocol = "anthropic"
model = "claude-sonnet-4-20250514"

[tools]

[agent]
"#;
        let config =
            run_load_config_with_env(global_toml, &[("OPENAI_API_KEY", Some("sk-openai-123"))])
                .unwrap();
        assert_eq!(
            config.llm.models[0].api_key.as_deref(),
            Some("sk-openai-123")
        );
        // anthropic 模型不应被 openai key 回填
        assert_eq!(config.llm.models[1].api_key, None);
    }

    const ENV_REF_TOML: &str = r#"
[daemon]
listen_addr = "[::1]:50051"

[llm]
[[llm.models]]
name = "DeepSeek"
protocol = "openai"
model = "deepseek-chat"
api_key = "${VISP_TEST_REF_KEY}"

[[llm.models]]
name = "Literal"
protocol = "openai"
model = "gpt-4o"
api_key = "sk-literal"

[tools]

[agent]
"#;

    #[test]
    #[serial]
    fn test_api_key_env_reference_is_expanded() {
        let config =
            run_load_config_with_env(ENV_REF_TOML, &[("VISP_TEST_REF_KEY", Some("sk-from-env"))])
                .unwrap();
        // safety: 测试中单线程执行（#[serial]）
        unsafe { std::env::remove_var("VISP_TEST_REF_KEY") };
        assert_eq!(config.llm.models[0].api_key.as_deref(), Some("sk-from-env"));
        assert_eq!(config.llm.models[1].api_key.as_deref(), Some("sk-literal"));
    }

    #[test]
    #[serial]
    fn test_api_key_env_reference_unset_falls_back_to_provider_env() {
        let config = run_load_config_with_env(
            ENV_REF_TOML,
            &[
                ("VISP_TEST_REF_KEY", None),
                ("OPENAI_API_KEY", Some("sk-openai-fallback")),
            ],
        )
        .unwrap();
        assert_eq!(
            config.llm.models[0].api_key.as_deref(),
            Some("sk-openai-fallback"),
            "an unset ${{VAR}} must not be sent literally; it falls back like a missing api_key"
        );
        assert_eq!(config.llm.models[1].api_key.as_deref(), Some("sk-literal"));
    }

    #[test]
    fn test_env_var_reference_parsing() {
        assert_eq!(
            env_var_reference("${OPENAI_API_KEY}"),
            Some("OPENAI_API_KEY")
        );
        assert_eq!(env_var_reference(" ${K1} "), Some("K1"));
        assert_eq!(env_var_reference("sk-abc"), None);
        assert_eq!(env_var_reference("${}"), None);
        assert_eq!(env_var_reference("${A-B}"), None);
        assert_eq!(env_var_reference("prefix-${A}"), None);
    }

    #[test]
    #[serial]
    fn test_env_anthropic_api_key_backfill() {
        let global_toml = r#"
[daemon]
listen_addr = "[::1]:50051"

[llm]
[[llm.models]]
name = "Sonnet"
protocol = "anthropic"
model = "claude-sonnet-4-20250514"

[[llm.models]]
name = "GPT-4o"
protocol = "openai"
model = "gpt-4o"

[tools]

[agent]
"#;
        let config =
            run_load_config_with_env(global_toml, &[("ANTHROPIC_API_KEY", Some("sk-ant-123"))])
                .unwrap();
        assert_eq!(config.llm.models[0].api_key.as_deref(), Some("sk-ant-123"));
        // openai 模型不应被 anthropic key 回填
        assert_eq!(config.llm.models[1].api_key, None);
    }

    #[test]
    #[serial]
    fn test_env_openai_api_key_backfill_by_provider_name() {
        // provider 字段包含 "openai"（大小写不敏感）也能匹配
        let global_toml = r#"
[daemon]
listen_addr = "[::1]:50051"

[llm]
[[llm.models]]
name = "DeepSeek"
protocol = "custom"
provider = "OpenAI-compatible"
model = "deepseek-v3"

[tools]

[agent]
"#;
        let config =
            run_load_config_with_env(global_toml, &[("OPENAI_API_KEY", Some("sk-env"))]).unwrap();
        assert_eq!(config.llm.models[0].api_key.as_deref(), Some("sk-env"));
    }

    #[test]
    #[serial]
    fn test_env_api_key_does_not_override_existing() {
        // 已显式配置 api_key 的模型不应被环境变量覆盖
        let global_toml = r#"
[daemon]
listen_addr = "[::1]:50051"

[llm]
[[llm.models]]
name = "GPT-4o"
protocol = "openai"
model = "gpt-4o"
api_key = "configured-key"

[tools]

[agent]
"#;
        let config =
            run_load_config_with_env(global_toml, &[("OPENAI_API_KEY", Some("env-key"))]).unwrap();
        assert_eq!(
            config.llm.models[0].api_key.as_deref(),
            Some("configured-key")
        );
    }

    #[test]
    fn soft_limit_config() {
        let toml = r#"
[daemon]
listen_addr = "[::1]:50051"

[llm]

[tools]

[agent]
soft_limit = 30
"#;
        let config: DaemonConfig = toml::from_str(toml).unwrap();
        assert_eq!(config.agent.soft_limit, 30);
    }

    #[test]
    fn doom_loop_threshold_config() {
        let toml = r#"
[daemon]
listen_addr = "[::1]:50051"

[llm]

[tools]

[agent]
doom_loop_threshold = 3
"#;
        let config: DaemonConfig = toml::from_str(toml).unwrap();
        assert_eq!(config.agent.doom_loop_threshold, 3);
    }

    #[test]
    fn backward_compat_max_iterations() {
        let toml = r#"
[daemon]
listen_addr = "[::1]:50051"

[llm]

[tools]

[agent]
max_iterations = 50
"#;
        let config: DaemonConfig = toml::from_str(toml).unwrap();
        assert_eq!(config.agent.soft_limit, 50);
    }

    #[test]
    fn default_doom_loop_threshold() {
        let config = default_config();
        assert_eq!(config.agent.doom_loop_threshold, 5);
    }

    #[test]
    fn test_storage_default_sqlite() {
        let config = default_config();
        assert_eq!(config.storage.driver, "sqlite");
    }

    #[test]
    fn test_storage_default_path() {
        let config = default_config();
        assert_eq!(config.storage.path, "~/.visp/data/visp.db");
    }

    #[test]
    fn test_storage_memory_mode() {
        let toml = r#"
[daemon]
listen_addr = "[::1]:50051"

[llm]

[tools]

[agent]

[storage]
driver = "memory"
"#;
        let config: DaemonConfig = toml::from_str(toml).unwrap();
        assert_eq!(config.storage.driver, "memory");
    }

    #[test]
    fn test_storage_custom_path() {
        let toml = r#"
[daemon]
listen_addr = "[::1]:50051"

[llm]

[tools]

[agent]

[storage]
driver = "sqlite"
path = "/tmp/custom/visp.db"
"#;
        let config: DaemonConfig = toml::from_str(toml).unwrap();
        assert_eq!(config.storage.path, "/tmp/custom/visp.db");
    }

    #[test]
    fn test_observability_config_default() {
        let config = default_config();
        assert!(config.observability.enabled);
        assert_eq!(config.observability.level, "info");
        assert_eq!(config.observability.format, "json");
        assert!(config.observability.parent_link);
        assert!(config.observability.metrics_summary);
        assert_eq!(config.observability.log_file, Some("~/.visp/logs".into()));
    }

    #[test]
    fn test_observability_config_disabled_via_toml() {
        let toml = r#"
[daemon]
listen_addr = "[::1]:50051"

[llm]

[tools]

[agent]

[observability]
enabled = false
"#;
        let config: DaemonConfig = toml::from_str(toml).unwrap();
        assert!(!config.observability.enabled);
        assert_eq!(config.observability.level, "info");
        assert_eq!(config.observability.format, "json");
        assert!(config.observability.parent_link);
        assert!(config.observability.metrics_summary);
        assert_eq!(config.observability.log_file, Some("~/.visp/logs".into()));
    }

    #[test]
    fn test_observability_config_full_override_via_toml() {
        let toml = r#"
[daemon]
listen_addr = "[::1]:50051"

[llm]

[tools]

[agent]

[observability]
enabled = false
level = "debug"
format = "text"
parent_link = false
metrics_summary = false
log_file = "/tmp/test.log"
"#;
        let config: DaemonConfig = toml::from_str(toml).unwrap();
        assert!(!config.observability.enabled);
        assert_eq!(config.observability.level, "debug");
        assert_eq!(config.observability.format, "text");
        assert!(!config.observability.parent_link);
        assert!(!config.observability.metrics_summary);
        assert_eq!(config.observability.log_file, Some("/tmp/test.log".into()));
    }

    #[test]
    fn test_observability_config_log_file_path() {
        let toml = r#"
[daemon]
listen_addr = "[::1]:50051"

[llm]

[tools]

[agent]

[observability]
log_file = "/tmp/test.log"
"#;
        let config: DaemonConfig = toml::from_str(toml).unwrap();
        assert_eq!(config.observability.log_file, Some("/tmp/test.log".into()));
    }

    #[test]
    fn test_daemon_config_default_includes_observability() {
        let config = default_config();
        assert_eq!(config.observability, ObservabilityConfig::default());
    }

    #[test]
    fn test_otlp_config_defaults_disabled() {
        let cfg = OtlpConfig::default();
        assert!(!cfg.enabled);
        assert_eq!(cfg.endpoint, "http://localhost:4317");
        assert_eq!(cfg.protocol, "grpc");
        assert_eq!(cfg.timeout_secs, 10);
        assert!(cfg.headers.is_empty());
        assert!((cfg.sample_rate - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_otlp_config_deserializes_from_toml() {
        let toml = r#"
[daemon]
listen_addr = "[::1]:50051"

[llm]

[tools]

[agent]

[observability]
enabled = true

[observability.otlp]
enabled = true
endpoint = "http://otel.example.com:4318"
protocol = "http/protobuf"
timeout_secs = 30
sample_rate = 0.1

[observability.otlp.headers]
x-api-key = "abc123"
environment = "test"
"#;
        let config: DaemonConfig = toml::from_str(toml).unwrap();
        assert!(config.observability.otlp.enabled);
        assert_eq!(
            config.observability.otlp.endpoint,
            "http://otel.example.com:4318"
        );
        assert_eq!(config.observability.otlp.protocol, "http/protobuf");
        assert_eq!(config.observability.otlp.timeout_secs, 30);
        assert!((config.observability.otlp.sample_rate - 0.1).abs() < f64::EPSILON);
        assert_eq!(
            config.observability.otlp.headers.get("x-api-key").unwrap(),
            "abc123"
        );
        assert_eq!(
            config
                .observability
                .otlp
                .headers
                .get("environment")
                .unwrap(),
            "test"
        );
    }

    #[test]
    fn test_otlp_config_omitted_section_is_default() {
        let toml = r#"
[daemon]
listen_addr = "[::1]:50051"

[llm]

[tools]

[agent]

[observability]
enabled = true
"#;
        let config: DaemonConfig = toml::from_str(toml).unwrap();
        assert_eq!(config.observability.otlp, OtlpConfig::default());
    }

    #[test]
    fn test_otlp_config_headers_kv_pairs() {
        let toml = r#"
[daemon]
listen_addr = "[::1]:50051"

[llm]

[tools]

[agent]

[observability.otlp.headers]
a = "1"
b = "2"
c = "3"
"#;
        let config: DaemonConfig = toml::from_str(toml).unwrap();
        let keys: Vec<&str> = config
            .observability
            .otlp
            .headers
            .keys()
            .map(|s| s.as_str())
            .collect();
        assert_eq!(keys, vec!["a", "b", "c"]);
    }

    #[test]
    fn test_otlp_config_sample_rate_clamped() {
        // Below 0.0 should clamp to 0.0
        let toml = r#"
[daemon]
listen_addr = "[::1]:50051"

[llm]

[tools]

[agent]

[observability.otlp]
sample_rate = -0.5
"#;
        let config: DaemonConfig = toml::from_str(toml).unwrap();
        assert!((config.observability.otlp.sample_rate - 0.0).abs() < f64::EPSILON);

        // Above 1.0 should clamp to 1.0
        let toml2 = r#"
[daemon]
listen_addr = "[::1]:50051"

[llm]

[tools]

[agent]

[observability.otlp]
sample_rate = 2.5
"#;
        let config2: DaemonConfig = toml::from_str(toml2).unwrap();
        assert!((config2.observability.otlp.sample_rate - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_observability_langfuse_config() {
        let toml = r#"
[daemon]
listen_addr = "[::1]:50051"

[llm]

[tools]

[agent]

[observability]
enabled = true

[observability.langfuse]
user_id = "user_456"
tags = ["agent", "weather"]
"#;
        let config: DaemonConfig = toml::from_str(toml).unwrap();
        assert_eq!(
            config.observability.langfuse.user_id.as_deref(),
            Some("user_456")
        );
        assert_eq!(
            config.observability.langfuse.tags,
            vec!["agent".to_string(), "weather".to_string()]
        );
    }

    #[test]
    fn test_observability_langfuse_defaults() {
        let config = default_config();
        assert!(!config.observability.langfuse.enabled);
        assert_eq!(config.observability.langfuse.user_id, None);
        assert!(config.observability.langfuse.tags.is_empty());
        assert_eq!(config.observability.langfuse.environment, None);
        assert_eq!(config.observability.langfuse.release, None);
        assert_eq!(config.observability.langfuse.version, None);
        assert_eq!(config.observability.langfuse.public, None);
        assert_eq!(config.observability.langfuse.metadata, None);
        assert!(!config.observability.langfuse.capture.input);
        assert!(!config.observability.langfuse.capture.output);
        assert_eq!(config.observability.langfuse.capture.max_chars, 20000);
        assert!(config.observability.langfuse.capture.redact_secrets);
    }

    #[test]
    fn test_observability_langfuse_full_capture_config() {
        let toml = r#"
[daemon]
listen_addr = "[::1]:50051"

[llm]

[tools]

[agent]

[observability.langfuse.capture]
input = true
output = true
max_chars = 10000
redact_secrets = false
"#;
        let config: DaemonConfig = toml::from_str(toml).unwrap();
        let capture = &config.observability.langfuse.capture;
        assert!(capture.input);
        assert!(capture.output);
        assert_eq!(capture.max_chars, 10000);
        assert!(!capture.redact_secrets);
    }

    #[test]
    fn test_observability_langfuse_omitted_section_is_default() {
        let toml = r#"
[daemon]
listen_addr = "[::1]:50051"

[llm]

[tools]

[agent]

[observability]
enabled = true
"#;
        let config: DaemonConfig = toml::from_str(toml).unwrap();
        assert_eq!(config.observability.langfuse, LangfuseConfig::default());
    }

    // ── 1a: 扩展 Langfuse 配置 ──────────────────────────────────────────

    #[test]
    fn test_langfuse_config_default() {
        let cfg = LangfuseConfig::default();
        assert!(!cfg.enabled);
        assert_eq!(cfg.user_id, None);
        assert!(cfg.tags.is_empty());
        assert_eq!(cfg.environment, None);
        assert_eq!(cfg.release, None);
        assert_eq!(cfg.version, None);
        assert_eq!(cfg.public, None);
        assert_eq!(cfg.metadata, None);
        assert!(!cfg.capture.input);
        assert!(!cfg.capture.output);
        assert_eq!(cfg.capture.max_chars, 20000);
        assert!(cfg.capture.redact_secrets);
    }

    #[test]
    fn test_langfuse_config_full() {
        let toml = r#"
[daemon]
listen_addr = "[::1]:50051"

[llm]

[tools]

[agent]

[observability.langfuse]
enabled = true
user_id = "user_456"
tags = ["agent", "weather"]
environment = "production"
release = "1.0.0"
version = "abc123"
public = true

[observability.langfuse.metadata]
env = "prod"
count = 42
items = ["a", "b"]

[observability.langfuse.metadata.nested]
key = "val"

[observability.langfuse.capture]
input = true
output = true
max_chars = 15000
redact_secrets = false
"#;
        let config: DaemonConfig = toml::from_str(toml).unwrap();
        let lf = &config.observability.langfuse;
        assert!(lf.enabled);
        assert_eq!(lf.user_id.as_deref(), Some("user_456"));
        assert_eq!(lf.tags, vec!["agent", "weather"]);
        assert_eq!(lf.environment.as_deref(), Some("production"));
        assert_eq!(lf.release.as_deref(), Some("1.0.0"));
        assert_eq!(lf.version.as_deref(), Some("abc123"));
        assert_eq!(lf.public, Some(true));
        let meta = lf.metadata.as_ref().unwrap();
        assert_eq!(meta.get("env").and_then(|v| v.as_str()), Some("prod"));
        assert!(meta.contains_key("count"));
        assert!(meta.contains_key("items"));
        assert!(meta.contains_key("nested"));
        assert!(lf.capture.input);
        assert!(lf.capture.output);
        assert_eq!(lf.capture.max_chars, 15000);
        assert!(!lf.capture.redact_secrets);
    }

    #[test]
    fn test_langfuse_config_public_tri_state() {
        // not set → None
        let config = default_config();
        assert_eq!(config.observability.langfuse.public, None);

        // false → Some(false)
        let toml_false = r#"
[daemon]
listen_addr = "[::1]:50051"

[llm]

[tools]

[agent]

[observability.langfuse]
public = false
"#;
        let cfg_false: DaemonConfig = toml::from_str(toml_false).unwrap();
        assert_eq!(cfg_false.observability.langfuse.public, Some(false));

        // true → Some(true)
        let toml_true = r#"
[daemon]
listen_addr = "[::1]:50051"

[llm]

[tools]

[agent]

[observability.langfuse]
public = true
"#;
        let cfg_true: DaemonConfig = toml::from_str(toml_true).unwrap();
        assert_eq!(cfg_true.observability.langfuse.public, Some(true));
    }

    #[test]
    fn test_langfuse_config_public_non_bool_error() {
        let toml = r#"
[daemon]
listen_addr = "[::1]:50051"

[llm]

[tools]

[agent]

[observability.langfuse]
public = "notabool"
"#;
        let result: Result<DaemonConfig, toml::de::Error> = toml::from_str(toml);
        assert!(result.is_err(), "non-boolean public should fail to parse");
    }

    #[test]
    fn test_langfuse_config_default_disabled() {
        let config = default_config();
        assert!(!config.observability.langfuse.enabled);
    }

    // ── 6a: Collector 示例与示例配置 ─────────────────────────────────────

    /// Helper: 从 crate 目录回溯到 workspace docs 目录
    fn docs_dir() -> PathBuf {
        let crate_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        // crate_dir = .../visp/crates/visp-daemon
        crate_dir
            .parent()
            .expect("crates")
            .parent()
            .expect("visp")
            .join("docs")
    }

    #[test]
    fn test_example_collector_yaml_exists() {
        let path = docs_dir().join("otel-collector-langfuse.example.yaml");
        assert!(
            path.exists(),
            "Collector 示例文件不存在: {}",
            path.display()
        );
    }

    #[test]
    fn test_example_collector_yaml_content() {
        let path = docs_dir().join("otel-collector-langfuse.example.yaml");
        let content = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("读取 {} 失败: {}", path.display(), e));

        // 必须包含 OTLP gRPC receiver
        assert!(content.contains("grpc"), "Collector 示例缺少 gRPC receiver");
        // 必包含 OTLP HTTP exporter
        assert!(
            content.contains("http/protobuf"),
            "Collector 示例缺少 OTLP HTTP exporter（http/protobuf）"
        );
        // 必包含 x-langfuse-ingestion-version=4
        assert!(
            content.contains("x-langfuse-ingestion-version") && content.contains("4"),
            "Collector 示例缺少 x-langfuse-ingestion-version=4"
        );
    }

    #[test]
    fn test_example_collector_yaml_no_real_secrets() {
        let path = docs_dir().join("otel-collector-langfuse.example.yaml");
        let content = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("读取 {} 失败: {}", path.display(), e));

        // 检查 secret key 行：如果有 sk-lf- 必须是占位符 ${...} 或明显示例值（含 example/test/your）
        for line in content.lines() {
            if line.contains("sk-lf-") {
                let trimmed = line.trim();
                assert!(
                    trimmed.contains('$')
                        || trimmed.contains("example")
                        || trimmed.contains("YOUR_")
                        || trimmed.contains("your-")
                        || trimmed.contains("your_"),
                    "Collector 示例包含疑似真实 sk-lf- secret: {}",
                    trimmed
                );
            }
            if line.contains("pk-lf-") {
                let trimmed = line.trim();
                assert!(
                    trimmed.contains('$')
                        || trimmed.contains("example")
                        || trimmed.contains("YOUR_")
                        || trimmed.contains("your-")
                        || trimmed.contains("your_"),
                    "Collector 示例包含疑似真实 pk-lf- public key: {}",
                    trimmed
                );
            }
        }
    }

    #[test]
    fn test_example_daemon_toml_has_langfuse_config() {
        let path = docs_dir().join("daemon.example.toml");
        let content = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("读取 {} 失败: {}", path.display(), e));

        // 必须包含 observability.langfuse 配置段
        assert!(
            content.contains("[observability.langfuse]"),
            "daemon.example.toml 缺少 [observability.langfuse] 段"
        );

        // 必须包含关键配置字段
        let expected_fields = [
            "enabled",
            "user_id",
            "tags",
            "environment",
            "release",
            "version",
            "public",
            "metadata",
            "capture",
        ];
        for field in &expected_fields {
            assert!(
                content.contains(field),
                "daemon.example.toml langfuse 配置缺少字段: {}",
                field
            );
        }
    }

    #[test]
    fn test_example_daemon_toml_collector_disclaimer() {
        let path = docs_dir().join("daemon.example.toml");
        let content = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("读取 {} 失败: {}", path.display(), e));

        // 必须声明 Collector 示例是参考配置
        let keywords = ["Collector", "参考", "示例", "管理"];
        let found = keywords.iter().any(|k| content.contains(k));
        assert!(
            found,
            "daemon.example.toml 未声明 Collector 示例为参考配置（缺少 Collector/参考/示例/管理 关键词）"
        );
    }

    #[test]
    fn test_builtin_agent_config_parsing() {
        let toml = r#"
[daemon]
listen_addr = "[::1]:50051"

[llm]
[[llm.models]]
name = "test"
protocol = "openai"
model = "gpt-4"

[tools]

[agent]
soft_limit = 50

[[agent.builtin]]
name = "explorer"
model = "Opencode/deepseek-v4-flash"
temperature = 0.1

[[agent.builtin]]
name = "fixer"
model = "Anthropic/claude-sonnet-4-20250514"
temperature = 0.2
steps = 30
"#;
        let config: DaemonConfig = toml::from_str(toml).unwrap();
        assert_eq!(config.agent.builtin.len(), 2);

        let explorer = &config.agent.builtin[0];
        assert_eq!(explorer.name, "explorer");
        assert_eq!(
            explorer.model.as_deref(),
            Some("Opencode/deepseek-v4-flash")
        );
        assert!((explorer.temperature.unwrap() - 0.1).abs() < f32::EPSILON);
        assert!(explorer.steps.is_none());

        let fixer = &config.agent.builtin[1];
        assert_eq!(fixer.name, "fixer");
        assert_eq!(
            fixer.model.as_deref(),
            Some("Anthropic/claude-sonnet-4-20250514")
        );
        assert_eq!(fixer.steps, Some(30));
    }

    #[test]
    fn test_builtin_agent_config_defaults_empty() {
        let toml = r#"
[daemon]
listen_addr = "[::1]:50051"

[llm]
[[llm.models]]
name = "test"
protocol = "openai"
model = "gpt-4"

[tools]

[agent]
soft_limit = 50
"#;
        let config: DaemonConfig = toml::from_str(toml).unwrap();
        assert!(config.agent.builtin.is_empty());
    }

    // ── resolve_default_key ────────────────────────────────────

    fn make_model_configs() -> Vec<LlmModelConfig> {
        vec![
            LlmModelConfig {
                name: "ModelA".into(),
                protocol: "anthropic".into(),
                provider: Some("ProviderA".into()),
                model: "model-a-api".into(),
                api_key: None,
                base_url: None,
                temperature: None,
                max_tokens: None,
                max_context_tokens: None,
                thinking_budget_tokens: None,
                use_tool: None,
                image_generation: None,
                extra: Default::default(),
            },
            LlmModelConfig {
                name: "ModelB".into(),
                protocol: "openai".into(),
                provider: Some("ProviderB".into()),
                model: "model-b-api".into(),
                api_key: None,
                base_url: None,
                temperature: None,
                max_tokens: None,
                max_context_tokens: None,
                thinking_budget_tokens: None,
                use_tool: None,
                image_generation: None,
                extra: Default::default(),
            },
        ]
    }

    fn empty_llm_section() -> LlmSection {
        LlmSection {
            thinking_budget_tokens: None,
            extra: std::collections::HashMap::new(),
            models: vec![],
            default: None,
            image_generation_model: None,
            vision_model: None,
        }
    }

    #[test]
    fn test_resolve_default_key_matches_default() {
        let models = make_model_configs();
        let section = LlmSection {
            default: Some("ProviderB/ModelB".into()),
            ..empty_llm_section()
        };
        assert_eq!(section.resolve_default_key(&models), "ProviderB/ModelB");
    }

    #[test]
    fn test_resolve_default_key_not_found_falls_back_to_first() {
        let models = make_model_configs();
        let section = LlmSection {
            default: Some("Unknown/Model".into()),
            ..empty_llm_section()
        };
        // default 不匹配任何模型，回退到第一个
        assert_eq!(section.resolve_default_key(&models), "ProviderA/ModelA");
    }

    #[test]
    fn test_resolve_default_key_none_falls_back_to_first() {
        let models = make_model_configs();
        let section = LlmSection {
            default: None,
            ..empty_llm_section()
        };
        assert_eq!(section.resolve_default_key(&models), "ProviderA/ModelA");
    }

    #[test]
    fn test_resolve_default_key_empty_models() {
        let section = LlmSection {
            default: None,
            ..empty_llm_section()
        };
        assert_eq!(section.resolve_default_key(&[]), "default");
    }

    // ── model_alias / matches_key ───────────────────────────────

    #[test]
    fn test_model_alias_format() {
        let mc = LlmModelConfig {
            name: "Display Name".into(),
            protocol: "anthropic".into(),
            provider: Some("ProviderX".into()),
            model: "api-model-v1".into(),
            ..make_model_configs()[0].clone()
        };
        assert_eq!(mc.model_alias(), "ProviderX/api-model-v1");
    }

    #[test]
    fn test_model_alias_falls_back_to_protocol() {
        let mc = LlmModelConfig {
            name: "Display Name".into(),
            protocol: "anthropic".into(),
            provider: None,
            model: "api-model-v1".into(),
            ..make_model_configs()[0].clone()
        };
        assert_eq!(mc.model_alias(), "anthropic/api-model-v1");
    }

    #[test]
    fn test_matches_key_by_key() {
        let models = make_model_configs();
        assert!(models[0].matches_key("ProviderA/ModelA"));
    }

    #[test]
    fn test_matches_key_by_model_alias() {
        let models = make_model_configs();
        assert!(models[0].matches_key("ProviderA/model-a-api"));
    }

    #[test]
    fn test_matches_key_no_match() {
        let models = make_model_configs();
        assert!(!models[0].matches_key("Unknown/Model"));
    }

    // ── resolve_default_key with model_alias ────────────────────

    #[test]
    fn test_resolve_default_key_matches_model_alias() {
        let models = make_model_configs();
        let section = LlmSection {
            default: Some("ProviderB/model-b-api".into()),
            ..empty_llm_section()
        };
        // 使用 {provider}/{model} 格式也能匹配
        assert_eq!(section.resolve_default_key(&models), "ProviderB/ModelB");
    }

    // ── save_config tests ────────────────────────────────────

    fn make_test_config() -> DaemonConfig {
        let toml_str = r#"
[daemon]
listen_addr = "[::1]:50051"

[llm]
default = "Anthropic/Sonnet"
[[llm.models]]
name = "Sonnet"
protocol = "anthropic"
model = "claude-sonnet-4-20250514"
api_key = "sk-test-key"
base_url = "https://api.anthropic.com"

[agent]
soft_limit = 30
"#;
        toml::from_str(toml_str).unwrap()
    }

    #[test]
    fn test_save_config_creates_file() {
        let dir = tempfile::TempDir::new().unwrap();
        let config = make_test_config();
        save_config(&config, dir.path()).unwrap();

        let target = dir.path().join(".visp/daemon.toml");
        assert!(target.exists(), "daemon.toml should exist");

        let content = std::fs::read_to_string(&target).unwrap();
        assert!(content.contains("[daemon]"));
        assert!(content.contains("[llm]"));
        assert!(content.contains("claude-sonnet-4-20250514"));
    }

    #[test]
    fn test_save_config_creates_dir() {
        let dir = tempfile::TempDir::new().unwrap();
        // .visp/ 不存在
        assert!(!dir.path().join(".visp").exists());

        let config = make_test_config();
        save_config(&config, dir.path()).unwrap();

        assert!(dir.path().join(".visp/daemon.toml").exists());
    }

    #[test]
    fn test_save_config_overwrites() {
        let dir = tempfile::TempDir::new().unwrap();

        // 先写入旧内容
        let config1 = make_test_config();
        save_config(&config1, dir.path()).unwrap();

        // 修改后再次写入
        let mut config2 = make_test_config();
        config2.daemon.listen_addr = "[::1]:99999".into();
        save_config(&config2, dir.path()).unwrap();

        let content = std::fs::read_to_string(dir.path().join(".visp/daemon.toml")).unwrap();
        assert!(content.contains("99999"));
        assert!(!content.contains("50051"));
    }

    #[test]
    fn test_save_config_preserves_api_key() {
        let dir = tempfile::TempDir::new().unwrap();
        let config = make_test_config();
        save_config(&config, dir.path()).unwrap();

        let content = std::fs::read_to_string(dir.path().join(".visp/daemon.toml")).unwrap();
        assert!(
            content.contains("sk-test-key"),
            "api_key should be preserved"
        );
    }

    #[test]
    fn test_save_config_roundtrip() {
        let dir = tempfile::TempDir::new().unwrap();
        let config = make_test_config();
        save_config(&config, dir.path()).unwrap();

        let target = dir.path().join(".visp/daemon.toml");
        let loaded = load_from_file(&target).unwrap();

        assert_eq!(loaded.daemon.listen_addr, config.daemon.listen_addr);
        assert_eq!(loaded.llm.default, config.llm.default);
        assert_eq!(loaded.llm.models.len(), 1);
        assert_eq!(loaded.llm.models[0].model, "claude-sonnet-4-20250514");
        assert_eq!(loaded.llm.models[0].api_key.as_deref(), Some("sk-test-key"));
        assert_eq!(loaded.agent.soft_limit, 30);
    }

    #[test]
    fn test_save_config_no_temp_residue() {
        let dir = tempfile::TempDir::new().unwrap();
        let config = make_test_config();
        save_config(&config, dir.path()).unwrap();

        // 不应残留临时文件
        let visp_dir = dir.path().join(".visp");
        let entries: Vec<_> = std::fs::read_dir(&visp_dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert!(
            !entries.iter().any(|n| n.ends_with(".visp-tmp")),
            "temp file residue found: {entries:?}"
        );
    }

    #[test]
    fn test_save_config_toml_parseable() {
        let dir = tempfile::TempDir::new().unwrap();
        let config = make_test_config();
        save_config(&config, dir.path()).unwrap();

        let content = std::fs::read_to_string(dir.path().join(".visp/daemon.toml")).unwrap();
        // 可被 toml::from_str 正常解析
        let parsed: DaemonConfig = toml::from_str(&content).unwrap();
        assert_eq!(parsed.llm.models.len(), 1);
    }
}

#[cfg(test)]
mod tests_llmconfig {
    use super::*;

    #[test]
    fn test_llmconfig_default() {
        let config = LlmConfig::default();
        assert_eq!(config.model, "claude-3-7-sonnet-20250219");
        assert_eq!(config.temperature, 0.7);
        assert_eq!(config.max_tokens, 4096);
        assert!(config.extra.is_empty());
    }

    #[test]
    fn test_llmconfig_capture_defaults() {
        let config = LlmConfig::default();
        assert!(
            !config.langfuse_capture_input,
            "capture_input should default to false"
        );
        assert!(
            !config.langfuse_capture_output,
            "capture_output should default to false"
        );
        assert_eq!(config.langfuse_capture_max_chars, 20000);
        assert!(config.langfuse_redact_secrets);
    }

    #[test]
    fn test_llmconfig_capture_configured() {
        let config = LlmConfig {
            langfuse_capture_input: true,
            langfuse_capture_output: false,
            langfuse_capture_max_chars: 5000,
            langfuse_redact_secrets: false,
            ..Default::default()
        };
        assert!(config.langfuse_capture_input);
        assert!(!config.langfuse_capture_output);
        assert_eq!(config.langfuse_capture_max_chars, 5000);
        assert!(!config.langfuse_redact_secrets);
    }

    #[test]
    fn test_llmconfig_extra() {
        let mut config = LlmConfig::default();
        config.extra.insert("key".to_string(), "value".to_string());
        assert_eq!(config.extra.get("key").unwrap(), "value");
    }

    #[test]
    fn test_llmconfig_default_max_context_tokens() {
        let config = LlmConfig::default();
        assert_eq!(config.max_context_tokens, 128_000);
    }

    #[test]
    fn test_llmconfig_custom_max_context_tokens() {
        let config = LlmConfig {
            max_context_tokens: 64_000,
            ..Default::default()
        };
        assert_eq!(config.max_context_tokens, 64_000);
    }

    // ── Langfuse trace 级字段测试 ──────────────────────────────────────────

    #[test]
    fn test_llmconfig_langfuse_trace_defaults() {
        let config = LlmConfig::default();
        assert!(
            !config.langfuse_enabled,
            "langfuse_enabled should default to false"
        );
        assert_eq!(config.langfuse_session_id, None);
        assert_eq!(config.langfuse_trace_name, None);
        assert_eq!(config.langfuse_user_id, None);
        assert_eq!(config.langfuse_tags, None);
        assert_eq!(config.langfuse_environment, None);
        assert_eq!(config.langfuse_release, None);
        assert_eq!(config.langfuse_version, None);
        assert_eq!(config.langfuse_public, None);
        assert_eq!(config.langfuse_metadata, None);
    }

    #[test]
    fn test_llmconfig_langfuse_trace_configured() {
        let mut meta = HashMap::new();
        meta.insert("env".into(), "prod".into());
        let config = LlmConfig {
            langfuse_enabled: true,
            langfuse_session_id: Some("sess_abc".into()),
            langfuse_trace_name: Some("visp.agent.run".into()),
            langfuse_user_id: Some("user_789".into()),
            langfuse_tags: Some(r#"["agent"]"#.into()),
            langfuse_environment: Some("staging".into()),
            langfuse_release: Some("1.0.0".into()),
            langfuse_version: Some("abc123".into()),
            langfuse_public: Some(true),
            langfuse_metadata: Some(meta.clone()),
            ..Default::default()
        };
        assert!(config.langfuse_enabled);
        assert_eq!(config.langfuse_session_id.as_deref(), Some("sess_abc"));
        assert_eq!(
            config.langfuse_trace_name.as_deref(),
            Some("visp.agent.run")
        );
        assert_eq!(config.langfuse_user_id.as_deref(), Some("user_789"));
        assert_eq!(config.langfuse_tags.as_deref(), Some(r#"["agent"]"#));
        assert_eq!(config.langfuse_environment.as_deref(), Some("staging"));
        assert_eq!(config.langfuse_release.as_deref(), Some("1.0.0"));
        assert_eq!(config.langfuse_version.as_deref(), Some("abc123"));
        assert_eq!(config.langfuse_public, Some(true));
        assert_eq!(config.langfuse_metadata, Some(meta));
    }

    #[test]
    fn test_llmconfig_langfuse_partial_trace() {
        let config = LlmConfig {
            langfuse_enabled: true,
            langfuse_user_id: Some("partial".into()),
            langfuse_environment: Some("default".into()),
            ..Default::default()
        };
        assert!(config.langfuse_enabled);
        assert_eq!(config.langfuse_user_id.as_deref(), Some("partial"));
        assert_eq!(config.langfuse_environment.as_deref(), Some("default"));
        // Other fields should remain None
        assert_eq!(config.langfuse_session_id, None);
        assert_eq!(config.langfuse_trace_name, None);
        assert_eq!(config.langfuse_tags, None);
        assert_eq!(config.langfuse_release, None);
        assert_eq!(config.langfuse_version, None);
        assert_eq!(config.langfuse_public, None);
        assert_eq!(config.langfuse_metadata, None);
    }

    #[test]
    fn test_llmconfig_langfuse_capture_defaults_unchanged() {
        // Verify existing capture defaults are not affected by new trace fields
        let config = LlmConfig::default();
        assert!(!config.langfuse_capture_input);
        assert!(!config.langfuse_capture_output);
        assert_eq!(config.langfuse_capture_max_chars, 20000);
        assert!(config.langfuse_redact_secrets);
    }
}

#[cfg(test)]
mod tests_runtime_config {
    use super::*;

    fn mc(name: &str, provider: &str, model: &str) -> LlmModelConfig {
        LlmModelConfig {
            name: name.into(),
            protocol: "openai".into(),
            provider: Some(provider.into()),
            model: model.into(),
            api_key: None,
            base_url: None,
            temperature: None,
            max_tokens: None,
            max_context_tokens: None,
            thinking_budget_tokens: None,
            use_tool: None,
            image_generation: None,
            extra: Default::default(),
        }
    }

    fn daemon_with(models: Vec<LlmModelConfig>, default: Option<&str>) -> DaemonConfig {
        let mut cfg = default_config();
        cfg.llm.models = models;
        cfg.llm.default = default.map(str::to_string);
        cfg
    }

    #[test]
    fn test_proto_to_llm_config_full() {
        let mut extra = HashMap::new();
        extra.insert("thinking_budget_tokens".into(), "2048".into());
        let proto = visp_proto::visp::LlmConfig {
            model: Some("deepseek-v4-flash".into()),
            model_key: Some("Opencode/DeepSeek v4 Flash".into()),
            temperature: Some(0.2),
            max_tokens: Some(8192),
            max_context_tokens: Some(64_000),
            extra: extra.clone(),
        };
        let config = proto_to_llm_config(&proto);
        assert_eq!(config.model, "deepseek-v4-flash");
        assert_eq!(
            config.model_key.as_deref(),
            Some("Opencode/DeepSeek v4 Flash")
        );
        assert_eq!(config.temperature, 0.2);
        assert_eq!(config.max_tokens, 8192);
        assert_eq!(config.max_context_tokens, 64_000);
        assert_eq!(config.extra, extra);
    }

    #[test]
    fn test_proto_to_llm_config_empty() {
        let config = proto_to_llm_config(&visp_proto::visp::LlmConfig::default());
        assert_eq!(config, LlmConfig::default());
    }

    #[test]
    fn test_proto_to_llm_config_partial() {
        let proto = visp_proto::visp::LlmConfig {
            model: Some("claude-3-7-sonnet-20250219".into()),
            ..Default::default()
        };
        let config = proto_to_llm_config(&proto);
        let default = LlmConfig::default();
        assert_eq!(config.model, "claude-3-7-sonnet-20250219");
        assert_eq!(config.model_key, None);
        assert_eq!(config.temperature, default.temperature);
        assert_eq!(config.max_tokens, default.max_tokens);
        assert_eq!(config.max_context_tokens, default.max_context_tokens);
        assert!(config.extra.is_empty());
    }

    #[test]
    fn test_llm_config_to_proto() {
        let mut extra = HashMap::new();
        extra.insert("top_p".into(), "0.9".into());
        let config = LlmConfig {
            model: "gpt-4o-mini".into(),
            model_key: Some("OpenAI/gpt-4o-mini".into()),
            provider: Some("OpenAI".into()),
            temperature: 0.1,
            max_tokens: 2048,
            max_context_tokens: 32_000,
            extra: extra.clone(),
            langfuse_enabled: true,
            langfuse_session_id: Some("sess_abc".into()),
            langfuse_trace_name: Some("visp.agent.run".into()),
            use_tool: false,
            image_generation: true,
            ..Default::default()
        };
        let proto = llm_config_to_proto(&config);
        // 6 fields mapped
        assert_eq!(proto.model.as_deref(), Some("gpt-4o-mini"));
        assert_eq!(proto.model_key.as_deref(), Some("OpenAI/gpt-4o-mini"));
        assert_eq!(proto.temperature, Some(0.1));
        assert_eq!(proto.max_tokens, Some(2048));
        assert_eq!(proto.max_context_tokens, Some(32_000));
        assert_eq!(proto.extra, extra);
        // 17 runtime-only fields dropped: proto 仅含上述 6 个字段，
        // langfuse/use_tool/image_generation 等不会泄漏到 proto。
        assert_eq!(proto.model, Some("gpt-4o-mini".into()));
    }

    #[test]
    fn test_model_config_to_info() {
        let mc = LlmModelConfig {
            name: "ModelA".into(),
            protocol: "anthropic".into(),
            provider: Some("ProviderA".into()),
            model: "model-a-api".into(),
            api_key: Some("sk-xxx".into()),
            base_url: Some("http://localhost:11434".into()),
            temperature: Some(0.3),
            max_tokens: Some(4096),
            max_context_tokens: Some(100_000),
            thinking_budget_tokens: Some(2048),
            use_tool: Some(false),
            image_generation: Some(true),
            extra: Default::default(),
        };
        let info = model_config_to_info(&mc);
        assert_eq!(info.model, "model-a-api");
        assert_eq!(info.provider.as_deref(), Some("ProviderA"));
        assert_eq!(info.temperature, Some(0.3));
        assert_eq!(info.max_tokens, Some(4096));
        assert_eq!(info.max_context_tokens, Some(100_000));
        assert!(info.image_generation);
        assert_eq!(info.use_tool, Some(false));
    }

    #[test]
    fn test_model_config_to_info_minimal() {
        let mc = LlmModelConfig {
            name: "ModelA".into(),
            protocol: "openai".into(),
            provider: None,
            model: "model-a-api".into(),
            api_key: None,
            base_url: None,
            temperature: None,
            max_tokens: None,
            max_context_tokens: None,
            thinking_budget_tokens: None,
            use_tool: None,
            image_generation: None,
            extra: Default::default(),
        };
        let info = model_config_to_info(&mc);
        assert_eq!(info.model, "model-a-api");
        assert_eq!(info.provider, None);
        assert_eq!(info.temperature, None);
        assert_eq!(info.max_tokens, None);
        assert_eq!(info.max_context_tokens, None);
        assert!(!info.image_generation);
        assert_eq!(info.use_tool, None);
    }

    #[test]
    fn test_resolve_model_found() {
        let cfg = daemon_with(vec![mc("ModelA", "ProviderA", "model-a-api")], None);
        let resolved = resolve_model("ProviderA/ModelA", &cfg).unwrap();
        assert_eq!(resolved.name, "ModelA");
        assert_eq!(resolved.model, "model-a-api");
        // model_alias 格式 {provider}/{model} 也能匹配
        let resolved_alias = resolve_model("ProviderA/model-a-api", &cfg).unwrap();
        assert_eq!(resolved_alias.name, "ModelA");
    }

    #[test]
    fn test_resolve_model_not_found() {
        let cfg = daemon_with(vec![mc("ModelA", "ProviderA", "model-a-api")], None);
        assert!(resolve_model("ProviderA/Missing", &cfg).is_none());
        assert!(resolve_model("ProviderB/ModelA", &cfg).is_none());
    }

    #[test]
    fn test_resolve_model_key_agent() {
        let cfg = daemon_with(
            vec![
                mc("ModelA", "ProviderA", "model-a-api"),
                mc("ModelB", "ProviderB", "model-b-api"),
            ],
            Some("ProviderA/ModelA"),
        );
        let session = LlmConfig {
            model_key: Some("ProviderB/ModelB".into()),
            ..Default::default()
        };
        // agent_model 优先级最高，即使 session 指定了别的 key
        assert_eq!(
            resolve_model_key(Some("ProviderA/ModelA"), &session, &cfg),
            "ProviderA/ModelA"
        );
    }

    #[test]
    fn test_resolve_model_key_session() {
        let cfg = daemon_with(
            vec![
                mc("ModelA", "ProviderA", "model-a-api"),
                mc("ModelB", "ProviderB", "model-b-api"),
            ],
            Some("ProviderA/ModelA"),
        );
        let session = LlmConfig {
            model_key: Some("ProviderB/ModelB".into()),
            ..Default::default()
        };
        // agent_model 缺失时回退到 session.model_key
        assert_eq!(resolve_model_key(None, &session, &cfg), "ProviderB/ModelB");
    }

    #[test]
    fn test_resolve_model_key_session_model() {
        let cfg = daemon_with(
            vec![mc("ModelA", "ProviderA", "model-a-api")],
            Some("ProviderA/ModelA"),
        );
        // 旧配置：session.model 直接就是 {provider}/{model} key
        let session = LlmConfig {
            model: "ProviderA/model-a-api".into(),
            model_key: None,
            ..Default::default()
        };
        assert_eq!(
            resolve_model_key(None, &session, &cfg),
            "ProviderA/model-a-api"
        );
    }

    #[test]
    fn test_resolve_model_key_default() {
        let cfg = daemon_with(
            vec![
                mc("ModelA", "ProviderA", "model-a-api"),
                mc("ModelB", "ProviderB", "model-b-api"),
            ],
            Some("ProviderA/ModelA"),
        );
        let session = LlmConfig {
            model: "nonexistent".into(),
            model_key: Some("ProviderX/Missing".into()),
            ..Default::default()
        };
        // 全部失败时回退到 llm.default
        assert_eq!(
            resolve_model_key(Some("ProviderY/Ghost"), &session, &cfg),
            "ProviderA/ModelA"
        );
    }

    #[test]
    fn test_apply_model_override_full() {
        let mc = LlmModelConfig {
            name: "ModelA".into(),
            protocol: "anthropic".into(),
            provider: Some("ProviderA".into()),
            model: "model-a-api".into(),
            api_key: None,
            base_url: None,
            temperature: Some(0.1),
            max_tokens: Some(2048),
            max_context_tokens: Some(50_000),
            thinking_budget_tokens: None,
            use_tool: Some(false),
            image_generation: Some(true),
            extra: Default::default(),
        };
        let mut config = LlmConfig::default();
        apply_model_override(&mut config, &mc);
        assert_eq!(config.model, "model-a-api");
        assert_eq!(config.provider.as_deref(), Some("ProviderA"));
        assert_eq!(config.temperature, 0.1);
        assert_eq!(config.max_tokens, 2048);
        assert_eq!(config.max_context_tokens, 50_000);
        assert!(config.image_generation);
        assert!(!config.use_tool);
    }

    #[test]
    fn test_apply_model_override_partial() {
        let mc = LlmModelConfig {
            name: "ModelA".into(),
            protocol: "anthropic".into(),
            provider: None,
            model: "model-a-api".into(),
            api_key: None,
            base_url: None,
            temperature: Some(0.5),
            max_tokens: None,
            max_context_tokens: None,
            thinking_budget_tokens: None,
            use_tool: None,
            image_generation: None,
            extra: Default::default(),
        };
        let mut config = LlmConfig {
            model: "old-model".into(),
            provider: Some("OldProvider".into()),
            temperature: 0.9,
            max_tokens: 999,
            max_context_tokens: 111_000,
            use_tool: false,
            image_generation: true,
            ..Default::default()
        };
        apply_model_override(&mut config, &mc);
        // model 非 Option，总是覆盖
        assert_eq!(config.model, "model-a-api");
        // 其余字段仅当 Some 时覆盖
        assert_eq!(config.provider.as_deref(), Some("OldProvider"));
        assert_eq!(config.temperature, 0.5);
        assert_eq!(config.max_tokens, 999);
        assert_eq!(config.max_context_tokens, 111_000);
        assert!(config.image_generation);
        assert!(!config.use_tool);
    }
}

#[cfg(test)]
mod tests_merge_build {
    use super::*;

    fn mc(
        name: &str,
        provider: &str,
        model: &str,
        temperature: Option<f64>,
        max_tokens: Option<u32>,
        max_context_tokens: Option<u32>,
        thinking_budget_tokens: Option<u32>,
    ) -> LlmModelConfig {
        LlmModelConfig {
            name: name.into(),
            protocol: "openai".into(),
            provider: Some(provider.into()),
            model: model.into(),
            api_key: None,
            base_url: None,
            temperature,
            max_tokens,
            max_context_tokens,
            thinking_budget_tokens,
            use_tool: None,
            image_generation: None,
            extra: Default::default(),
        }
    }

    fn daemon_with(models: Vec<LlmModelConfig>, default: Option<&str>) -> DaemonConfig {
        let mut cfg = default_config();
        cfg.llm.models = models;
        cfg.llm.default = default.map(str::to_string);
        cfg
    }

    #[test]
    fn test_merge_no_client_config() {
        let daemon = daemon_with(
            vec![mc(
                "Default",
                "ProviderDef",
                "model-default",
                Some(0.1),
                Some(2048),
                Some(64_000),
                Some(1024),
            )],
            Some("ProviderDef/Default"),
        );
        let config = merge_session_config(None, &daemon);
        // model/model_key/provider 来自 daemon 默认模型
        assert_eq!(config.model, "model-default");
        assert_eq!(config.model_key.as_deref(), Some("ProviderDef/Default"));
        assert_eq!(config.provider.as_deref(), Some("ProviderDef"));
        // extra 来自 daemon 默认（含默认模型的 thinking_budget_tokens）
        assert_eq!(
            config
                .extra
                .get("thinking_budget_tokens")
                .map(String::as_str),
            Some("1024")
        );
        // 温度/长度哨兵字段不被 daemon 默认覆盖（与 service.rs 行为一致）
        assert_eq!(config.temperature, LlmConfig::default().temperature);
        assert_eq!(config.max_tokens, LlmConfig::default().max_tokens);
    }

    #[test]
    fn test_merge_with_model_key() {
        let daemon = daemon_with(
            vec![
                mc(
                    "A",
                    "ProviderA",
                    "model-a",
                    Some(0.2),
                    Some(8192),
                    Some(100_000),
                    Some(2048),
                ),
                mc(
                    "Default",
                    "ProviderDef",
                    "model-default",
                    None,
                    None,
                    None,
                    None,
                ),
            ],
            Some("ProviderDef/Default"),
        );
        let proto = visp_proto::visp::LlmConfig {
            model_key: Some("ProviderA/A".into()),
            ..Default::default()
        };
        let config = merge_session_config(Some(&proto), &daemon);
        assert_eq!(config.model, "model-a");
        assert_eq!(config.model_key.as_deref(), Some("ProviderA/A"));
        assert_eq!(config.provider.as_deref(), Some("ProviderA"));
        // 哨兵值检测：客户端未设置 → 用 model 配置填充
        assert_eq!(config.temperature, 0.2);
        assert_eq!(config.max_tokens, 8192);
        assert_eq!(config.max_context_tokens, 100_000);
        assert_eq!(
            config
                .extra
                .get("thinking_budget_tokens")
                .map(String::as_str),
            Some("2048")
        );
    }

    #[test]
    fn test_merge_model_key_not_found() {
        let daemon = daemon_with(
            vec![mc(
                "Default",
                "ProviderDef",
                "model-default",
                Some(0.5),
                Some(3000),
                Some(90_000),
                Some(500),
            )],
            Some("ProviderDef/Default"),
        );
        let proto = visp_proto::visp::LlmConfig {
            model_key: Some("NoSuch/Model".into()),
            ..Default::default()
        };
        let config = merge_session_config(Some(&proto), &daemon);
        // model_key 未匹配 → model/provider 回退 daemon 默认
        assert_eq!(config.model, "model-default");
        assert_eq!(config.model_key.as_deref(), Some("NoSuch/Model"));
        assert_eq!(config.provider.as_deref(), Some("ProviderDef"));
        // 未走 model_key 解析分支，温度保持默认
        assert_eq!(config.temperature, 0.7);
    }

    #[test]
    fn test_merge_client_overrides_model() {
        let daemon = daemon_with(
            vec![mc(
                "Default",
                "ProviderDef",
                "model-default",
                Some(0.5),
                Some(3000),
                Some(90_000),
                None,
            )],
            Some("ProviderDef/Default"),
        );
        let proto = visp_proto::visp::LlmConfig {
            model: Some("custom-model".into()),
            ..Default::default()
        };
        let config = merge_session_config(Some(&proto), &daemon);
        // 客户端显式设置了 model → 不使用 daemon 默认（model_key/provider 保持 None）
        assert_eq!(config.model, "custom-model");
        assert_eq!(config.model_key, None);
        assert_eq!(config.provider, None);
        assert_eq!(config.temperature, 0.7);
    }

    #[test]
    fn test_merge_sentinel_temperature() {
        let daemon = daemon_with(
            vec![mc("A", "ProviderA", "model-a", Some(0.1), None, None, None)],
            Some("ProviderA/A"),
        );
        let proto = visp_proto::visp::LlmConfig {
            model_key: Some("ProviderA/A".into()),
            // 客户端显式设置了 max_tokens，temperature 未设置
            max_tokens: Some(1234),
            ..Default::default()
        };
        let config = merge_session_config(Some(&proto), &daemon);
        // temperature 是默认哨兵值 → 从 model 配置填充
        assert_eq!(config.temperature, 0.1);
        // 客户端显式设置的 max_tokens 不被覆盖
        assert_eq!(config.max_tokens, 1234);
    }

    #[test]
    fn test_merge_sentinel_max_tokens() {
        let daemon = daemon_with(
            vec![mc(
                "A",
                "ProviderA",
                "model-a",
                None,
                Some(7777),
                None,
                None,
            )],
            Some("ProviderA/A"),
        );
        let proto = visp_proto::visp::LlmConfig {
            model_key: Some("ProviderA/A".into()),
            // 客户端显式设置了 temperature，max_tokens 未设置
            temperature: Some(0.9),
            ..Default::default()
        };
        let config = merge_session_config(Some(&proto), &daemon);
        // max_tokens 是默认哨兵值 → 从 model 配置填充
        assert_eq!(config.max_tokens, 7777);
        // 客户端显式设置的 temperature 不被覆盖
        assert_eq!(config.temperature, 0.9);
    }

    #[test]
    fn test_merge_extra_thinking_budget() {
        let daemon = daemon_with(
            vec![mc(
                "A",
                "ProviderA",
                "model-a",
                None,
                None,
                None,
                Some(4096),
            )],
            Some("ProviderA/A"),
        );
        let proto = visp_proto::visp::LlmConfig {
            model_key: Some("ProviderA/A".into()),
            extra: [("seed".to_string(), "42".to_string())]
                .into_iter()
                .collect(),
            ..Default::default()
        };
        let config = merge_session_config(Some(&proto), &daemon);
        // thinking_budget_tokens 注入 extra，客户端自定义 extra 保留
        assert_eq!(
            config
                .extra
                .get("thinking_budget_tokens")
                .map(String::as_str),
            Some("4096")
        );
        assert_eq!(config.extra.get("seed").map(String::as_str), Some("42"));
    }

    #[test]
    fn test_build_llm_config_from_model_full() {
        let model_cfg = LlmModelConfig {
            name: "Full".into(),
            protocol: "anthropic".into(),
            provider: Some("Anthropic".into()),
            model: "claude-full-api".into(),
            api_key: None,
            base_url: None,
            temperature: Some(0.3),
            max_tokens: Some(9000),
            max_context_tokens: Some(200_000),
            thinking_budget_tokens: Some(2048),
            use_tool: None,
            image_generation: None,
            extra: Default::default(),
        };
        let obs = ObservabilityConfig {
            langfuse: LangfuseConfig {
                enabled: true,
                user_id: Some("user-1".into()),
                tags: vec!["tag1".into(), "tag2".into()],
                environment: Some("prod".into()),
                release: Some("1.0.0".into()),
                version: Some("v1".into()),
                public: Some(true),
                metadata: Some(
                    [("k".to_string(), toml::Value::String("v".into()))]
                        .into_iter()
                        .collect(),
                ),
                capture: LangfuseCaptureConfig {
                    input: true,
                    output: false,
                    max_chars: 5000,
                    redact_secrets: false,
                },
            },
            ..Default::default()
        };
        let config = build_llm_config_from_model(&model_cfg, Some(&obs));
        assert_eq!(config.model, "claude-full-api");
        assert_eq!(config.model_key.as_deref(), Some("Anthropic/Full"));
        assert_eq!(config.provider.as_deref(), Some("Anthropic"));
        assert_eq!(config.temperature, 0.3);
        assert_eq!(config.max_tokens, 9000);
        assert_eq!(config.max_context_tokens, 200_000);
        assert_eq!(
            config
                .extra
                .get("thinking_budget_tokens")
                .map(String::as_str),
            Some("2048")
        );
        assert!(config.langfuse_enabled);
        assert_eq!(config.langfuse_user_id.as_deref(), Some("user-1"));
        assert_eq!(config.langfuse_tags.as_deref(), Some(r#"["tag1","tag2"]"#));
        assert_eq!(config.langfuse_environment.as_deref(), Some("prod"));
        assert_eq!(config.langfuse_release.as_deref(), Some("1.0.0"));
        assert_eq!(config.langfuse_version.as_deref(), Some("v1"));
        assert_eq!(config.langfuse_public, Some(true));
        assert_eq!(
            config
                .langfuse_metadata
                .as_ref()
                .and_then(|m| m.get("k"))
                .map(String::as_str),
            Some("v")
        );
        assert!(config.langfuse_capture_input);
        assert!(!config.langfuse_capture_output);
        assert_eq!(config.langfuse_capture_max_chars, 5000);
        assert!(!config.langfuse_redact_secrets);
        // per-session 字段保持 None
        assert_eq!(config.langfuse_session_id, None);
        assert_eq!(config.langfuse_trace_name, None);
    }

    #[test]
    fn test_build_llm_config_from_model_no_langfuse() {
        let model_cfg = LlmModelConfig {
            name: "NoLangfuse".into(),
            protocol: "openai".into(),
            provider: None,
            model: "plain-model".into(),
            api_key: None,
            base_url: None,
            temperature: None,
            max_tokens: None,
            max_context_tokens: None,
            thinking_budget_tokens: None,
            use_tool: None,
            image_generation: None,
            extra: Default::default(),
        };
        let config = build_llm_config_from_model(&model_cfg, None);
        assert_eq!(config.model, "plain-model");
        // provider 缺省时回退 protocol
        assert_eq!(config.provider.as_deref(), Some("openai"));
        assert_eq!(config.model_key.as_deref(), Some("openai/NoLangfuse"));
        // 未提供数值时使用 LlmConfig 默认值
        assert_eq!(config.temperature, 0.7);
        assert_eq!(config.max_tokens, 4096);
        assert_eq!(config.max_context_tokens, 128_000);
        assert!(config.extra.is_empty());
        // langfuse 全部为禁用默认值
        assert!(!config.langfuse_enabled);
        assert_eq!(config.langfuse_user_id, None);
        assert_eq!(config.langfuse_tags, None);
        assert_eq!(config.langfuse_metadata, None);
        assert_eq!(config.langfuse_public, None);
        assert!(!config.langfuse_capture_input);
        assert!(!config.langfuse_capture_output);
        assert_eq!(config.langfuse_capture_max_chars, 20_000);
        assert!(config.langfuse_redact_secrets);
    }
}

#[cfg(test)]
mod tests_apply_config_update {
    use super::*;

    fn mc(
        name: &str,
        provider: &str,
        model: &str,
        temperature: Option<f64>,
        max_tokens: Option<u32>,
        max_context_tokens: Option<u32>,
        thinking_budget_tokens: Option<u32>,
    ) -> LlmModelConfig {
        LlmModelConfig {
            name: name.into(),
            protocol: "openai".into(),
            provider: Some(provider.into()),
            model: model.into(),
            api_key: None,
            base_url: None,
            temperature,
            max_tokens,
            max_context_tokens,
            thinking_budget_tokens,
            use_tool: None,
            image_generation: None,
            extra: Default::default(),
        }
    }

    fn daemon_with(models: Vec<LlmModelConfig>) -> DaemonConfig {
        let mut cfg = default_config();
        cfg.llm.models = models;
        cfg
    }

    #[test]
    fn test_apply_config_update_model_key() {
        let daemon = daemon_with(vec![mc(
            "A",
            "ProviderA",
            "model-a",
            Some(0.2),
            Some(8192),
            Some(100_000),
            Some(2048),
        )]);
        // current：model_key/provider 与默认不同，temperature/max_tokens 等为默认哨兵值
        let current = LlmConfig {
            model: "old-model".into(),
            model_key: Some("Old/Model".into()),
            provider: Some("OldProvider".into()),
            temperature: LlmConfig::default().temperature,
            max_tokens: LlmConfig::default().max_tokens,
            max_context_tokens: LlmConfig::default().max_context_tokens,
            langfuse_enabled: true,
            langfuse_session_id: Some("sess-1".into()),
            use_tool: false,
            image_generation: true,
            ..LlmConfig::default()
        };

        let update = visp_proto::visp::LlmConfig {
            model_key: Some("ProviderA/A".into()),
            ..Default::default()
        };

        let config = apply_config_update(&current, &update, &daemon);

        // model/model_key/provider 来自匹配的 daemon 模型
        assert_eq!(config.model, "model-a");
        assert_eq!(config.model_key.as_deref(), Some("ProviderA/A"));
        assert_eq!(config.provider.as_deref(), Some("ProviderA"));
        // 哨兵值检测：current 未设置非默认值 → 用模型配置填充
        assert_eq!(config.temperature, 0.2);
        assert_eq!(config.max_tokens, 8192);
        assert_eq!(config.max_context_tokens, 100_000);
        assert_eq!(
            config
                .extra
                .get("thinking_budget_tokens")
                .map(String::as_str),
            Some("2048")
        );
        // 其余字段从 current 保留
        assert!(config.langfuse_enabled);
        assert_eq!(config.langfuse_session_id.as_deref(), Some("sess-1"));
        assert!(!config.use_tool);
        assert!(config.image_generation);
    }

    #[test]
    fn test_apply_config_update_temperature() {
        let daemon = daemon_with(vec![]);
        let current = LlmConfig {
            model: "keep-model".into(),
            model_key: Some("Keep/Model".into()),
            provider: Some("KeepProvider".into()),
            temperature: 0.5,
            max_tokens: 2048,
            max_context_tokens: 32_000,
            langfuse_enabled: true,
            langfuse_session_id: Some("sess-2".into()),
            use_tool: false,
            ..LlmConfig::default()
        };

        let update = visp_proto::visp::LlmConfig {
            temperature: Some(1.3),
            ..Default::default()
        };

        let config = apply_config_update(&current, &update, &daemon);

        assert_eq!(config.temperature, 1.3);
        // 其余字段保留
        assert_eq!(config.model, "keep-model");
        assert_eq!(config.model_key.as_deref(), Some("Keep/Model"));
        assert_eq!(config.provider.as_deref(), Some("KeepProvider"));
        assert_eq!(config.max_tokens, 2048);
        assert_eq!(config.max_context_tokens, 32_000);
        assert!(config.langfuse_enabled);
        assert_eq!(config.langfuse_session_id.as_deref(), Some("sess-2"));
        assert!(!config.use_tool);
    }

    #[test]
    fn test_apply_config_update_model_key_not_found() {
        let daemon = daemon_with(vec![mc(
            "A",
            "ProviderA",
            "model-a",
            Some(0.2),
            Some(8192),
            Some(100_000),
            Some(2048),
        )]);
        let current = LlmConfig {
            model: "orig-model".into(),
            model_key: Some("Orig/Model".into()),
            provider: Some("OrigProvider".into()),
            temperature: 0.4,
            max_tokens: 1500,
            max_context_tokens: 40_000,
            langfuse_enabled: true,
            use_tool: false,
            ..LlmConfig::default()
        };

        let update = visp_proto::visp::LlmConfig {
            model_key: Some("NoSuch/Model".into()),
            ..Default::default()
        };

        let config = apply_config_update(&current, &update, &daemon);

        // 未匹配 → 保留原配置（model/provider/temperature 等不变）
        assert_eq!(config.model, "orig-model");
        assert_eq!(config.model_key.as_deref(), Some("NoSuch/Model"));
        assert_eq!(config.provider.as_deref(), Some("OrigProvider"));
        assert_eq!(config.temperature, 0.4);
        assert_eq!(config.max_tokens, 1500);
        assert_eq!(config.max_context_tokens, 40_000);
        assert!(config.langfuse_enabled);
        assert!(!config.use_tool);
    }

    #[test]
    fn test_apply_config_update_partial() {
        let daemon = daemon_with(vec![]);
        let current = LlmConfig {
            model: "base-model".into(),
            model_key: Some("Base/Model".into()),
            provider: Some("BaseProvider".into()),
            temperature: 0.7,
            max_tokens: 4096,
            max_context_tokens: 128_000,
            langfuse_enabled: true,
            langfuse_user_id: Some("user-1".into()),
            ..LlmConfig::default()
        };

        let update = visp_proto::visp::LlmConfig {
            model: Some("new-model".into()),
            temperature: Some(1.1),
            max_tokens: Some(6000),
            ..Default::default()
        };

        let config = apply_config_update(&current, &update, &daemon);

        // 仅更新 update 中显式设置的字段
        assert_eq!(config.model, "new-model");
        assert_eq!(config.temperature, 1.1);
        assert_eq!(config.max_tokens, 6000);
        // 其余字段从 current 保留
        assert_eq!(config.model_key.as_deref(), Some("Base/Model"));
        assert_eq!(config.provider.as_deref(), Some("BaseProvider"));
        assert_eq!(config.max_context_tokens, 128_000);
        assert!(config.langfuse_enabled);
        assert_eq!(config.langfuse_user_id.as_deref(), Some("user-1"));
    }
}

// ---------------------------------------------------------------------------
// [notification] 通知配置段
// ---------------------------------------------------------------------------
#[cfg(test)]
mod tests_notification {
    use super::*;

    /// 1. 未配置 [notification] 段 → 全字段默认值
    #[test]
    fn notification_defaults_when_section_absent() {
        let config: DaemonConfig = toml::from_str("").unwrap();
        let n = &config.notification;
        assert!(n.enabled);
        assert!(n.on_complete);
        assert!(n.on_user_input);
        assert_eq!(n.protocol, NotificationProtocol::Auto);
        assert_eq!(n.min_interval_secs, 3);
    }

    /// 2. enabled = false 显式解析生效，未写的字段仍取默认值
    #[test]
    fn notification_enabled_false() {
        let config: DaemonConfig = toml::from_str("[notification]\nenabled = false\n").unwrap();
        assert!(!config.notification.enabled);
        assert!(config.notification.on_complete);
        assert!(config.notification.on_user_input);
    }

    /// 3. protocol 四个取值均可解析为对应变体
    #[test]
    fn notification_protocol_all_values() {
        for (raw, expected) in [
            ("auto", NotificationProtocol::Auto),
            ("osc9", NotificationProtocol::Osc9),
            ("osc777", NotificationProtocol::Osc777),
            ("kitty99", NotificationProtocol::Kitty99),
        ] {
            let toml_str = format!("[notification]\nprotocol = \"{}\"\n", raw);
            let config: DaemonConfig = toml::from_str(&toml_str).unwrap();
            assert_eq!(config.notification.protocol, expected, "protocol = {}", raw);
        }
    }

    /// 4. 非法 protocol 值 → 反序列化失败
    #[test]
    fn notification_protocol_invalid() {
        let result = toml::from_str::<DaemonConfig>("[notification]\nprotocol = \"osc123\"\n");
        assert!(result.is_err(), "非法 protocol 值应当反序列化失败");
    }

    /// 5. on_complete / on_user_input 两开关独立生效
    #[test]
    fn notification_switches_independent() {
        let config: DaemonConfig =
            toml::from_str("[notification]\non_complete = false\non_user_input = true\n").unwrap();
        assert!(!config.notification.on_complete);
        assert!(config.notification.on_user_input);
    }

    /// 6. min_interval_secs = 0 合法（表示不限流）
    #[test]
    fn notification_zero_interval() {
        let config: DaemonConfig =
            toml::from_str("[notification]\nmin_interval_secs = 0\n").unwrap();
        assert_eq!(config.notification.min_interval_secs, 0);
    }

    /// 7. DaemonConfig::default() 含通知段默认值
    #[test]
    fn notification_default_impl() {
        let config = DaemonConfig::default();
        assert!(config.notification.enabled);
        assert!(config.notification.on_complete);
        assert!(config.notification.on_user_input);
        assert_eq!(config.notification.protocol, NotificationProtocol::Auto);
        assert_eq!(config.notification.min_interval_secs, 3);
    }

    /// 8. Serialize → Deserialize 往返一致（save_config 路径不丢字段）
    #[test]
    fn notification_roundtrip() {
        let section = NotificationSection {
            enabled: false,
            on_complete: true,
            on_user_input: false,
            protocol: NotificationProtocol::Kitty99,
            min_interval_secs: 7,
        };
        let toml_str = toml::to_string(&section).unwrap();
        let parsed: NotificationSection = toml::from_str(&toml_str).unwrap();
        assert_eq!(parsed.enabled, section.enabled);
        assert_eq!(parsed.on_complete, section.on_complete);
        assert_eq!(parsed.on_user_input, section.on_user_input);
        assert_eq!(parsed.protocol, section.protocol);
        assert_eq!(parsed.min_interval_secs, section.min_interval_secs);
    }
}

#[cfg(test)]
mod tests_daemon_filewatcher {
    use super::*;

    /// 1. 配置缺省 → 自动文件监听开关默认开启（设计 §7 决策 12）。
    #[test]
    fn filewatcher_defaults_enabled() {
        let config: DaemonConfig = toml::from_str("").unwrap();
        assert!(config.daemon.filewatcher);
        assert!(default_config().daemon.filewatcher);
    }

    /// 2. 显式置 false → 解析为关闭；未写的其他 daemon 字段仍取默认值。
    #[test]
    fn filewatcher_explicit_false() {
        let config: DaemonConfig = toml::from_str("[daemon]\nfilewatcher = false\n").unwrap();
        assert!(!config.daemon.filewatcher);
        assert_eq!(config.daemon.listen_addr, default_listen_addr());
        assert_eq!(config.daemon.log_level, default_log_level());
    }
}
