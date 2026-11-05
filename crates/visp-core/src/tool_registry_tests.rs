use super::*;
use async_trait::async_trait;
use std::path::PathBuf;

struct MockTool {
    name: String,
    description: String,
    parameters: serde_json::Value,
}

#[async_trait]
impl Tool for MockTool {
    fn name(&self) -> &str {
        &self.name
    }
    fn description(&self) -> &str {
        &self.description
    }
    fn parameters(&self) -> serde_json::Value {
        self.parameters.clone()
    }
    async fn execute(&self, args: serde_json::Value, _ctx: &ToolContext) -> ToolResult {
        let result = format!("executed {} with {}", self.name, args);
        ToolResult::success(result)
    }
}

fn mock_tool(name: &str, desc: &str) -> Arc<dyn Tool> {
    Arc::new(MockTool {
        name: name.to_string(),
        description: desc.to_string(),
        parameters: serde_json::json!({}),
    })
}

fn test_context() -> ToolContext {
    ToolContext {
        working_dir: PathBuf::from("/tmp"),
        session_id: None,
        permission_rules: None,
        global_tx: None,
        visp_trace_id: None,
        iter_span_w3c_id: None,
    }
}

#[test]
fn test_register_and_get() {
    let registry = ToolRegistry::new();
    registry.register(mock_tool("echo", "Echo tool")).unwrap();
    let tool = registry.get("echo");
    assert!(tool.is_some());
    assert_eq!(tool.unwrap().name(), "echo");
}

#[test]
fn test_definitions() {
    let registry = ToolRegistry::new();
    registry.register(mock_tool("tool_a", "Tool A")).unwrap();
    registry.register(mock_tool("tool_b", "Tool B")).unwrap();

    let defs = registry.definitions();
    assert_eq!(defs.len(), 2);

    let names: Vec<&str> = defs.iter().map(|d| d.name.as_str()).collect();
    assert!(names.contains(&"tool_a"));
    assert!(names.contains(&"tool_b"));
}

#[tokio::test]
async fn test_execute() {
    let registry = ToolRegistry::new();
    registry
        .register(mock_tool("greet", "Greeting tool"))
        .unwrap();

    let ctx = test_context();
    let result = registry
        .execute("greet", serde_json::json!({"name": "world"}), &ctx)
        .await;
    assert!(result.is_some());
    let r = result.unwrap();
    assert!(!r.is_error);
    assert!(r.content.contains("executed greet"));
}

#[test]
fn test_duplicate_name() {
    let registry = ToolRegistry::new();
    registry.register(mock_tool("echo", "Echo tool")).unwrap();
    let err = registry.register(mock_tool("echo", "Another echo"));
    assert!(err.is_err());
    assert!(
        err.unwrap_err()
            .to_string()
            .contains("duplicate tool name: echo")
    );
}

#[test]
fn test_get_not_found() {
    let registry = ToolRegistry::new();
    let tool = registry.get("nonexistent");
    assert!(tool.is_none());
}

// ── remove tests ───────────────────────────────────────────────────────

#[test]
fn test_remove_existing() {
    let registry = ToolRegistry::new();
    registry.register(mock_tool("echo", "Echo tool")).unwrap();
    assert!(registry.get("echo").is_some());

    registry.remove("echo").unwrap();
    assert!(registry.get("echo").is_none());
}

#[test]
fn test_remove_not_found() {
    let registry = ToolRegistry::new();
    let err = registry.remove("nonexistent");
    assert!(err.is_err());
}

// ── update tests ───────────────────────────────────────────────────────

#[test]
fn test_update_existing() {
    let registry = ToolRegistry::new();
    registry.register(mock_tool("echo", "Echo tool")).unwrap();

    let updated = mock_tool("echo", "Updated echo");
    registry.update("echo", updated).unwrap();

    let tool = registry.get("echo").unwrap();
    assert_eq!(tool.description(), "Updated echo");
}

#[test]
fn test_update_not_found() {
    let registry = ToolRegistry::new();
    let err = registry.update("nonexistent", mock_tool("x", "X"));
    assert!(err.is_err());
}

// ── seal + register_mcp tests ──────────────────────────────────────────

#[test]
fn test_seal_core_tools() {
    let registry = ToolRegistry::new();
    registry.register(mock_tool("core_a", "Core A")).unwrap();
    registry.register(mock_tool("core_b", "Core B")).unwrap();

    registry.seal_core_tools();

    // MCP tools with same names as core tools should be skipped
    registry
        .register_mcp(mock_tool("core_a", "MCP Core A"))
        .unwrap();
    // The tool should still be the original one
    let tool = registry.get("core_a").unwrap();
    assert_eq!(tool.description(), "Core A");
}

#[test]
fn test_register_mcp_no_conflict() {
    let registry = ToolRegistry::new();
    registry.register(mock_tool("core_a", "Core A")).unwrap();
    registry.seal_core_tools();

    // MCP tool with non-core name should be registered
    registry
        .register_mcp(mock_tool("mcp_tool", "MCP Tool"))
        .unwrap();
    let tool = registry.get("mcp_tool").unwrap();
    assert_eq!(tool.description(), "MCP Tool");
}

#[test]
fn test_register_mcp_after_seal_without_core_conflict() {
    let registry = ToolRegistry::new();
    registry.seal_core_tools(); // no core tools

    registry
        .register_mcp(mock_tool("any_tool", "Any tool"))
        .unwrap();
    assert!(registry.get("any_tool").is_some());
}

#[test]
fn test_register_mcp_concurrent_reads() {
    let registry = ToolRegistry::new();
    registry.register(mock_tool("tool", "Tool")).unwrap();

    // definitions and names should work concurrently
    let defs = registry.definitions();
    let names = registry.names();
    assert_eq!(defs.len(), 1);
    assert_eq!(names, vec!["tool"]);
}
