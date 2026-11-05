use super::*;

// ── merge_permissions ────────────────────────────────────────────────

#[test]
fn test_merge_both_empty_adds_deny_all() {
    let result = merge_permissions(&[], &[], &[]);
    assert_eq!(result.len(), 1);
    assert_eq!(result[0].permission, "*");
    assert_eq!(result[0].pattern, "*");
    assert_eq!(result[0].action, PermissionAction::Deny);
}

#[test]
fn test_merge_inherits_parent_session_deny() {
    let session_rules = vec![PermissionRule {
        permission: "edit".into(),
        pattern: "*".into(),
        action: PermissionAction::Deny,
    }];
    let result = merge_permissions(&session_rules, &[], &[]);
    assert!(
        result
            .iter()
            .any(|r| r.permission == "edit" && r.action == PermissionAction::Deny)
    );
}

#[test]
fn test_merge_inherits_parent_agent_deny() {
    let agent_rules = vec![PermissionRule {
        permission: "bash".into(),
        pattern: "*".into(),
        action: PermissionAction::Deny,
    }];
    let result = merge_permissions(&[], &agent_rules, &[]);
    assert!(
        result
            .iter()
            .any(|r| r.permission == "bash" && r.action == PermissionAction::Deny)
    );
}

#[test]
fn test_merge_subagent_allow_overrides_deny_all() {
    let sub_rules = vec![PermissionRule {
        permission: "read".into(),
        pattern: "*".into(),
        action: PermissionAction::Allow,
    }];
    let result = merge_permissions(&[], &[], &sub_rules);
    // Should have both the allow rule and the fallback deny-all
    assert!(
        result
            .iter()
            .any(|r| r.permission == "read" && r.action == PermissionAction::Allow)
    );
    assert!(
        result
            .iter()
            .any(|r| r.permission == "*" && r.action == PermissionAction::Deny)
    );
}

#[test]
fn test_merge_existing_deny_all_not_duplicated() {
    let sub_rules = vec![PermissionRule {
        permission: "*".into(),
        pattern: "*".into(),
        action: PermissionAction::Deny,
    }];
    let result = merge_permissions(&[], &[], &sub_rules);
    let deny_all_count = result
        .iter()
        .filter(|r| r.permission == "*" && r.pattern == "*" && r.action == PermissionAction::Deny)
        .count();
    assert_eq!(deny_all_count, 1);
}

#[test]
fn test_merge_explicit_deny_all_preserved() {
    let sub_rules = vec![PermissionRule {
        permission: "*".into(),
        pattern: "*".into(),
        action: PermissionAction::Deny,
    }];
    let result = merge_permissions(&[], &[], &sub_rules);
    assert!(
        result
            .iter()
            .any(|r| r.permission == "*" && r.pattern == "*" && r.action == PermissionAction::Deny)
    );
}

// ── check_permission ─────────────────────────────────────────────────

#[test]
fn test_check_exact_allow() {
    let rules = vec![PermissionRule {
        permission: "edit".into(),
        pattern: "*".into(),
        action: PermissionAction::Allow,
    }];
    assert_eq!(
        check_permission("edit", &serde_json::json!({}), &rules),
        PermissionDecision::Allowed
    );
}

#[test]
fn test_check_exact_deny() {
    let rules = vec![PermissionRule {
        permission: "bash".into(),
        pattern: "*".into(),
        action: PermissionAction::Deny,
    }];
    assert!(matches!(
        check_permission("bash", &serde_json::json!({}), &rules),
        PermissionDecision::Denied(_)
    ));
}

#[test]
fn test_check_wildcard_permission_match() {
    let rules = vec![PermissionRule {
        permission: "*".into(),
        pattern: "*".into(),
        action: PermissionAction::Allow,
    }];
    assert_eq!(
        check_permission("any_tool", &serde_json::json!({}), &rules),
        PermissionDecision::Allowed
    );
}

#[test]
fn test_check_exact_match_overrides_wildcard() {
    let rules = vec![
        PermissionRule {
            permission: "*".into(),
            pattern: "*".into(),
            action: PermissionAction::Allow,
        },
        PermissionRule {
            permission: "danger".into(),
            pattern: "*".into(),
            action: PermissionAction::Deny,
        },
    ];
    assert!(matches!(
        check_permission("danger", &serde_json::json!({}), &rules),
        PermissionDecision::Denied(_)
    ));
    assert_eq!(
        check_permission("safe", &serde_json::json!({}), &rules),
        PermissionDecision::Allowed
    );
}

// ── allowed_sub_agents ──────────────────────────────────────────────

#[test]
fn test_agent_definition_allowed_sub_agents_default_empty() {
    let def = AgentDefinition {
        name: "test".to_string(),
        description: String::new(),
        mode: AgentMode::Primary,
        model: None,
        temperature: None,
        steps: None,
        permission: vec![],
        system_prompt: String::new(),
        allowed_sub_agents: Vec::new(),
    };
    assert!(def.allowed_sub_agents.is_empty());
}

#[test]
fn test_agent_definition_with_allowed_sub_agents() {
    let def = AgentDefinition {
        name: "test".to_string(),
        description: String::new(),
        mode: AgentMode::Primary,
        model: None,
        temperature: None,
        steps: None,
        permission: vec![],
        system_prompt: String::new(),
        allowed_sub_agents: vec!["fixer".to_string(), "explorer".to_string()],
    };
    assert_eq!(def.allowed_sub_agents.len(), 2);
    assert!(def.allowed_sub_agents.contains(&"fixer".to_string()));
    assert!(def.allowed_sub_agents.contains(&"explorer".to_string()));
}

#[test]
fn test_check_no_match_default_allow() {
    let rules = vec![PermissionRule {
        permission: "edit".into(),
        pattern: "*".into(),
        action: PermissionAction::Deny,
    }];
    assert_eq!(
        check_permission("unknown_tool", &serde_json::json!({}), &rules),
        PermissionDecision::Allowed
    );
}

// ── AgentDefinition equality（reload 变化守卫的等值基础） ─────────────

fn base_definition() -> AgentDefinition {
    AgentDefinition {
        name: "coder".to_string(),
        description: "a coding agent".to_string(),
        mode: AgentMode::Primary,
        model: Some("gpt-4".to_string()),
        temperature: Some(0.2),
        steps: Some(30),
        permission: vec![PermissionRule {
            permission: "edit".into(),
            pattern: "*".into(),
            action: PermissionAction::Allow,
        }],
        allowed_sub_agents: vec!["fixer".to_string()],
        system_prompt: "you are a coder".to_string(),
    }
}

#[test]
fn test_agent_definition_equality_identical() {
    assert_eq!(base_definition(), base_definition());
}

#[test]
fn test_agent_definition_inequality_on_permission() {
    let mut other = base_definition();
    other.permission = vec![PermissionRule {
        permission: "edit".into(),
        pattern: "*".into(),
        action: PermissionAction::Deny,
    }];
    assert_ne!(base_definition(), other);
}

#[test]
fn test_agent_definition_inequality_on_system_prompt() {
    let mut other = base_definition();
    other.system_prompt = "you are a different coder".to_string();
    assert_ne!(base_definition(), other);
}

#[test]
fn test_agent_definition_inequality_on_optional_fields() {
    let mut without_model = base_definition();
    without_model.model = None;
    assert_ne!(base_definition(), without_model);

    let mut without_temperature = base_definition();
    without_temperature.temperature = None;
    assert_ne!(base_definition(), without_temperature);

    let mut without_steps = base_definition();
    without_steps.steps = None;
    assert_ne!(base_definition(), without_steps);
}

#[test]
fn test_agent_definition_permission_order_sensitive() {
    let rule = |permission: &str| PermissionRule {
        permission: permission.into(),
        pattern: "*".into(),
        action: PermissionAction::Allow,
    };

    let mut forward = base_definition();
    forward.permission = vec![rule("bash"), rule("edit")];

    let mut reversed = base_definition();
    reversed.permission = vec![rule("edit"), rule("bash")];

    assert_ne!(forward, reversed);
}
