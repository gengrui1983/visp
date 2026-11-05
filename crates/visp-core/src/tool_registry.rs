use std::collections::HashSet;
use std::sync::{Arc, RwLock};

use crate::error::CoreError;
use crate::message::ToolDefinition;
use crate::tool::{Tool, ToolContext, ToolResult};

/// 大小写不敏感的名称比较
fn name_eq(a: &str, b: &str) -> bool {
    a.eq_ignore_ascii_case(b)
}

#[derive(Default)]
pub struct ToolRegistry {
    tools: RwLock<Vec<Arc<dyn Tool>>>,
    core_tool_names: RwLock<HashSet<String>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }
}

impl ToolRegistry {
    pub fn register(&self, tool: Arc<dyn Tool>) -> Result<(), CoreError> {
        let name = tool.name().to_string();
        let mut tools = self.tools.write().unwrap();
        if tools.iter().any(|t| name_eq(t.name(), &name)) {
            return Err(CoreError::Tool(format!("duplicate tool name: {name}")));
        }
        tools.push(tool);
        Ok(())
    }

    /// 注册 MCP 工具，跳过与核心工具名称冲突的工具
    pub fn register_mcp(&self, tool: Arc<dyn Tool>) -> Result<(), CoreError> {
        let name = tool.name().to_string();
        let core_names: HashSet<String> = self
            .core_tool_names
            .read()
            .unwrap()
            .iter()
            .map(|n| n.to_lowercase())
            .collect();
        if core_names.contains(&name.to_lowercase()) {
            tracing::warn!("MCP tool '{name}' conflicts with built-in tool, skipping");
            return Ok(());
        }
        drop(core_names); // release read lock before taking write lock

        let mut tools = self.tools.write().unwrap();
        if tools.iter().any(|t| name_eq(t.name(), &name)) {
            tracing::warn!("MCP tool '{name}' conflicts with another registered tool, skipping");
            return Ok(());
        }
        tools.push(tool);
        Ok(())
    }

    /// 锁定当前已注册的所有工具名为核心工具
    /// 之后通过 register_mcp 注册的工具不能覆盖这些名称
    pub fn seal_core_tools(&self) {
        let tools = self.tools.read().unwrap();
        let mut core_names = self.core_tool_names.write().unwrap();
        for t in tools.iter() {
            core_names.insert(t.name().to_string());
        }
    }

    /// 移除已注册的工具
    pub fn remove(&self, name: &str) -> Result<(), CoreError> {
        let mut tools = self.tools.write().unwrap();
        let pos = tools.iter().position(|t| name_eq(t.name(), name));
        match pos {
            Some(i) => {
                tools.remove(i);
                Ok(())
            }
            None => Err(CoreError::Tool(format!("tool '{name}' not found"))),
        }
    }

    /// 更新/替换同名工具
    pub fn update(&self, name: &str, tool: Arc<dyn Tool>) -> Result<(), CoreError> {
        let mut tools = self.tools.write().unwrap();
        let pos = tools.iter().position(|t| name_eq(t.name(), name));
        match pos {
            Some(i) => {
                tools[i] = tool;
                Ok(())
            }
            None => Err(CoreError::Tool(format!("tool '{name}' not found"))),
        }
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        let tools = self.tools.read().unwrap();
        tools.iter().find(|t| name_eq(t.name(), name)).cloned()
    }

    pub fn definitions(&self) -> Vec<ToolDefinition> {
        let tools = self.tools.read().unwrap();
        tools
            .iter()
            .map(|t| ToolDefinition {
                name: t.name().to_string(),
                description: t.description().to_string(),
                parameters: t.parameters(),
                category: t.category().to_string(),
            })
            .collect()
    }

    pub fn names(&self) -> Vec<String> {
        let tools = self.tools.read().unwrap();
        tools.iter().map(|t| t.name().to_string()).collect()
    }

    pub async fn execute(
        &self,
        name: &str,
        args: serde_json::Value,
        ctx: &ToolContext,
    ) -> Option<ToolResult> {
        match self.get(name) {
            Some(tool) => Some(tool.execute(args, ctx).await),
            None => None,
        }
    }
}

#[cfg(test)]
#[path = "tool_registry_tests.rs"]
mod tests;
