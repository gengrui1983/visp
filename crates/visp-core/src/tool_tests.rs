use super::*;
use std::path::Path;

// ── tests_toolcontext ──

#[test]
fn test_toolcontext_default_session_id() {
    let ctx = ToolContext {
        working_dir: PathBuf::from("/tmp"),
        session_id: None,
        permission_rules: None,
        global_tx: None,
        visp_trace_id: None,
        iter_span_w3c_id: None,
    };
    assert_eq!(ctx.session_id, None);
    assert_eq!(ctx.working_dir, Path::new("/tmp"));
}

#[test]
fn test_tool_context_global_tx_none_by_default() {
    let ctx = ToolContext {
        working_dir: PathBuf::from("/tmp"),
        session_id: None,
        permission_rules: None,
        global_tx: None,
        visp_trace_id: None,
        iter_span_w3c_id: None,
    };
    assert!(ctx.global_tx.is_none());
}

#[test]
fn test_tool_context_trace_fields_none_by_default() {
    let ctx = ToolContext {
        working_dir: PathBuf::from("/tmp"),
        session_id: None,
        permission_rules: None,
        global_tx: None,
        visp_trace_id: None,
        iter_span_w3c_id: None,
    };
    assert_eq!(ctx.visp_trace_id, None);
    assert_eq!(ctx.iter_span_w3c_id, None);
}

// ── tests_tool_approval ──

struct NoApprovalTool;

#[async_trait]
impl Tool for NoApprovalTool {
    fn name(&self) -> &str {
        "no_approval"
    }
    fn description(&self) -> &str {
        "test tool"
    }
    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({})
    }
    async fn execute(&self, _args: serde_json::Value, _ctx: &ToolContext) -> ToolResult {
        ToolResult::success("ok")
    }
}

#[test]
fn test_tool_default_requires_approval() {
    let tool = NoApprovalTool;
    assert!(!tool.requires_approval());
}

#[test]
fn test_requires_approval_for_default_matches_approval() {
    let tool = NoApprovalTool;
    let args = serde_json::json!({"url": "https://example.com"});
    assert_eq!(tool.requires_approval_for(&args), tool.requires_approval());
}

struct ApprovalForTool;

#[async_trait]
impl Tool for ApprovalForTool {
    fn name(&self) -> &str {
        "approval_for"
    }
    fn description(&self) -> &str {
        "test tool with dynamic approval"
    }
    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({})
    }
    async fn execute(&self, _args: serde_json::Value, _ctx: &ToolContext) -> ToolResult {
        ToolResult::success("ok")
    }

    fn requires_approval_for(&self, arguments: &serde_json::Value) -> bool {
        // 模拟白名单：example.com 不需要确认，其他需要
        arguments
            .get("url")
            .and_then(|v| v.as_str())
            .map(|url| !url.contains("example.com"))
            .unwrap_or(true)
    }
}

#[test]
fn test_requires_approval_for_override_allowed() {
    let tool = ApprovalForTool;
    let args = serde_json::json!({"url": "https://example.com/page"});
    assert!(!tool.requires_approval_for(&args));
}

#[test]
fn test_requires_approval_for_override_denied() {
    let tool = ApprovalForTool;
    let args = serde_json::json!({"url": "https://evil.com"});
    assert!(tool.requires_approval_for(&args));
}

// ── category() tests ──────────────────────────────────────────────────────

#[test]
fn test_tool_default_category() {
    let tool = NoApprovalTool;
    assert_eq!(tool.category(), "other");
}

// ── tests_tool_type ──

struct BuiltinMockTool;

#[async_trait]
impl Tool for BuiltinMockTool {
    fn name(&self) -> &str {
        "builtin_mock"
    }
    fn description(&self) -> &str {
        "Builtin mock tool"
    }
    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({})
    }
    async fn execute(&self, _args: serde_json::Value, _ctx: &ToolContext) -> ToolResult {
        ToolResult::success("ok")
    }
}

struct McpMockTool;

#[async_trait]
impl Tool for McpMockTool {
    fn name(&self) -> &str {
        "mcp_mock"
    }
    fn description(&self) -> &str {
        "MCP mock tool"
    }
    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({})
    }
    async fn execute(&self, _args: serde_json::Value, _ctx: &ToolContext) -> ToolResult {
        ToolResult::success("ok")
    }
    fn tool_type(&self) -> ToolType {
        ToolType::Mcp
    }
}

struct AgentMockTool;

#[async_trait]
impl Tool for AgentMockTool {
    fn name(&self) -> &str {
        "agent_mock"
    }
    fn description(&self) -> &str {
        "Agent mock tool"
    }
    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({})
    }
    async fn execute(&self, _args: serde_json::Value, _ctx: &ToolContext) -> ToolResult {
        ToolResult::success("ok")
    }
    fn tool_type(&self) -> ToolType {
        ToolType::Agent
    }
}

struct SkillMockTool;

#[async_trait]
impl Tool for SkillMockTool {
    fn name(&self) -> &str {
        "skill_mock"
    }
    fn description(&self) -> &str {
        "Skill mock tool"
    }
    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({})
    }
    async fn execute(&self, _args: serde_json::Value, _ctx: &ToolContext) -> ToolResult {
        ToolResult::success("ok")
    }
    fn tool_type(&self) -> ToolType {
        ToolType::Skill
    }
}

#[test]
fn test_tool_type_default_is_builtin() {
    let tool = BuiltinMockTool;
    assert_eq!(tool.tool_type(), ToolType::Builtin);
}

#[test]
fn test_tool_type_mcp_returns_mcp() {
    let tool = McpMockTool;
    assert_eq!(tool.tool_type(), ToolType::Mcp);
}

#[test]
fn test_tool_type_agent_returns_agent() {
    let tool = AgentMockTool;
    assert_eq!(tool.tool_type(), ToolType::Agent);
}

#[test]
fn test_tool_type_skill_returns_skill() {
    let tool = SkillMockTool;
    assert_eq!(tool.tool_type(), ToolType::Skill);
}
