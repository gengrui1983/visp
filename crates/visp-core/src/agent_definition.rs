/// Agent 模式
#[derive(Debug, Clone, PartialEq)]
pub enum AgentMode {
    /// 主 Agent，用户可直接选择
    Primary,
    /// 子 Agent，仅通过 task 工具调用
    Subagent,
    /// 两者皆可
    All,
}

/// 权限动作
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum PermissionAction {
    Allow,
    Deny,
}

/// 权限规则三元组
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct PermissionRule {
    /// 工具名，如 "edit"，"*" 表示所有工具
    pub permission: String,
    /// 参数路径 glob，如 "*" 表示所有路径
    pub pattern: String,
    /// 允许或拒绝
    pub action: PermissionAction,
}

/// Agent 静态定义
#[derive(Debug, Clone, PartialEq)]
pub struct AgentDefinition {
    pub name: String,
    pub description: String,
    pub mode: AgentMode,
    /// 使用的模型 key（可选，未指定则继承 session 默认模型）
    pub model: Option<String>,
    pub temperature: Option<f32>,
    /// 最大 agentic 迭代次数
    pub steps: Option<u32>,
    /// 权限规则集
    pub permission: Vec<PermissionRule>,
    /// 允许调用的子 Agent 名称列表（空列表表示不限制，仅指业务逻辑层面）
    pub allowed_sub_agents: Vec<String>,
    /// 系统提示词
    pub system_prompt: String,
}

/// 权限检查结果
#[derive(Debug, Clone, PartialEq)]
pub enum PermissionDecision {
    Allowed,
    Denied(String),
}

/// 合并三组权限规则：父 session deny → 父 agent deny → 子 agent 规则 → 兜底 `*: deny`
pub fn merge_permissions(
    parent_session_permission: &[PermissionRule],
    parent_agent_permission: &[PermissionRule],
    subagent_permission: &[PermissionRule],
) -> Vec<PermissionRule> {
    let mut result = Vec::new();

    // 1. 父 session 的 deny 规则
    for rule in parent_session_permission {
        if rule.action == PermissionAction::Deny {
            result.push(rule.clone());
        }
    }

    // 2. 父 agent 的 deny 规则
    for rule in parent_agent_permission {
        if rule.action == PermissionAction::Deny {
            result.push(rule.clone());
        }
    }

    // 3. 子 agent 的所有规则
    result.extend(subagent_permission.iter().cloned());

    // 4. 兜底 *: deny（如果没有显式 *: deny）
    if !result
        .iter()
        .any(|r| r.permission == "*" && r.pattern == "*" && r.action == PermissionAction::Deny)
    {
        result.push(PermissionRule {
            permission: "*".to_string(),
            pattern: "*".to_string(),
            action: PermissionAction::Deny,
        });
    }

    result
}

/// 检查工具调用是否被允许
/// 两轮匹配：先精确匹配 permission，再通配匹配 "*"
/// 无匹配时默认 Allow
pub fn check_permission(
    name: &str,
    args: &serde_json::Value,
    rules: &[PermissionRule],
) -> PermissionDecision {
    let args_str = args.to_string();

    // 第一轮：精确匹配 permission
    for rule in rules {
        if rule.permission == name && (rule.pattern == "*" || args_str.contains(&rule.pattern)) {
            match rule.action {
                PermissionAction::Allow => return PermissionDecision::Allowed,
                PermissionAction::Deny => {
                    return PermissionDecision::Denied(format!(
                        "permission denied: tool '{}' is blocked by rule '{}'",
                        name, rule.permission
                    ));
                }
            }
        }
    }

    // 第二轮：通配匹配 permission="*"
    for rule in rules {
        if rule.permission == "*" && (rule.pattern == "*" || args_str.contains(&rule.pattern)) {
            match rule.action {
                PermissionAction::Allow => return PermissionDecision::Allowed,
                PermissionAction::Deny => {
                    return PermissionDecision::Denied(format!(
                        "permission denied: tool '{}' blocked by wildcard rule",
                        name
                    ));
                }
            }
        }
    }

    // 无匹配 → 默认 Allow
    PermissionDecision::Allowed
}

#[cfg(test)]
#[path = "agent_definition_tests.rs"]
mod tests;
