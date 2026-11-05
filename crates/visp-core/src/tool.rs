use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;

use crate::agent::Envelope;

use crate::agent_definition::PermissionRule;

/// 工具类型
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolType {
    /// 内置工具
    Builtin,
    /// MCP 工具
    Mcp,
    /// Agent 工具
    Agent,
    /// Skill 工具
    Skill,
}

/// 工具执行上下文
#[derive(Debug, Clone)]
pub struct ToolContext {
    /// 当前工作目录
    pub working_dir: PathBuf,
    /// 会话 ID（用于日志追踪和 bash 确认模式判断）
    pub session_id: Option<String>,
    /// 权限规则集（多 Agent 模式）
    pub permission_rules: Option<Arc<Vec<PermissionRule>>>,
    /// 全局事件总线发送端（用于子 Agent 向父 Agent 上报事件）
    pub global_tx: Option<tokio::sync::mpsc::Sender<Envelope>>,
    /// 追踪 ID（W3C traceparent 的 trace-id 部分）
    pub visp_trace_id: Option<String>,
    /// W3C traceparent 的 span-id（迭代级别），用于可观测性
    pub iter_span_w3c_id: Option<String>,
}

/// 工具执行结果
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResult {
    /// 结果内容（成功时为输出，失败时为错误描述）
    pub content: String,
    /// 是否为错误
    pub is_error: bool,
}

impl ToolResult {
    pub fn success(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: false,
        }
    }

    pub fn error(message: impl Into<String>) -> Self {
        Self {
            content: message.into(),
            is_error: true,
        }
    }
}

/// 工具抽象 trait
/// 所有工具（内置 + MCP）都必须实现此 trait
#[async_trait]
pub trait Tool: Send + Sync {
    /// 工具名称
    fn name(&self) -> &str;

    /// 工具描述（给 LLM 看的说明）
    fn description(&self) -> &str;

    /// 参数定义（JSON Schema 格式，用于 LLM function calling）
    fn parameters(&self) -> serde_json::Value;

    /// 执行工具
    async fn execute(&self, arguments: serde_json::Value, context: &ToolContext) -> ToolResult;

    /// 是否需要用户确认才能执行（默认 false）
    fn requires_approval(&self) -> bool {
        false
    }

    /// 根据参数判断是否需要用户确认（默认调用 requires_approval()）
    /// 工具可覆盖此方法以根据执行参数动态决定是否需要审批
    fn requires_approval_for(&self, _arguments: &serde_json::Value) -> bool {
        self.requires_approval()
    }

    /// 工具分类（用于动态工具指南的分组展示）
    /// 默认分类为 "other"，各工具可根据功能覆盖
    fn category(&self) -> &str {
        "other"
    }

    /// 工具类型（用于区分工具的来源/种类）
    /// 默认返回 Builtin，各工具可根据需要覆盖
    fn tool_type(&self) -> ToolType {
        ToolType::Builtin
    }
}

#[cfg(test)]
#[path = "tool_tests.rs"]
mod tests;
