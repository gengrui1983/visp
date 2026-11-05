use std::collections::HashMap;

use crate::agent_definition::{AgentDefinition, AgentMode};
use crate::error::CoreError;

/// Agent 注册中心
pub struct AgentRegistry {
    agents: HashMap<String, AgentDefinition>,
}

impl AgentRegistry {
    pub fn new() -> Self {
        Self {
            agents: HashMap::new(),
        }
    }

    /// 注册 agent，同名返回 Err
    pub fn register(&mut self, agent: AgentDefinition) -> Result<(), CoreError> {
        let name = agent.name.clone();
        if self.agents.contains_key(&name) {
            return Err(CoreError::Tool(format!("duplicate agent name: {name}")));
        }
        self.agents.insert(name, agent);
        Ok(())
    }

    /// 注册或替换 agent（同名时覆盖）
    pub fn register_or_replace(&mut self, agent: AgentDefinition) {
        self.agents.insert(agent.name.clone(), agent);
    }

    /// 按名称查找 agent
    pub fn get(&self, name: &str) -> Option<&AgentDefinition> {
        self.agents.get(name)
    }

    /// 按名称查找 agent（可变引用）
    pub fn get_mut(&mut self, name: &str) -> Option<&mut AgentDefinition> {
        self.agents.get_mut(name)
    }

    /// 获取默认 agent
    /// 优先取 mode != Subagent 且名称含 "default" 的 agent；
    /// 无则取第一个 mode != Subagent 的 agent；
    /// 全 Subagent 时返回 None
    pub fn default(&self) -> Option<&AgentDefinition> {
        // 优先名称为 default 的
        for agent in self.agents.values() {
            if agent.mode != AgentMode::Subagent && agent.name == "default" {
                return Some(agent);
            }
        }
        // 备选第一个 primary / all
        self.agents
            .values()
            .find(|&agent| agent.mode != AgentMode::Subagent)
    }

    /// 列出所有 agent
    pub fn list(&self) -> Vec<&AgentDefinition> {
        self.agents.values().collect()
    }

    /// 列出所有 subagent（mode == Subagent 或 All）
    pub fn list_subagents(&self) -> Vec<&AgentDefinition> {
        self.agents
            .values()
            .filter(|a| a.mode == AgentMode::Subagent || a.mode == AgentMode::All)
            .collect()
    }
}

impl Default for AgentRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
#[path = "agent_registry_tests.rs"]
mod tests;
