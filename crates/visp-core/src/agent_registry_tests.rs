use super::*;

fn make_agent(name: &str, mode: AgentMode) -> AgentDefinition {
    AgentDefinition {
        name: name.to_string(),
        description: String::new(),
        mode,
        model: None,
        temperature: None,
        steps: None,
        permission: vec![],
        allowed_sub_agents: Vec::new(),
        system_prompt: String::new(),
    }
}

#[test]
fn test_register_and_get() {
    let mut reg = AgentRegistry::new();
    reg.register(make_agent("code-review", AgentMode::Subagent))
        .unwrap();
    let agent = reg.get("code-review");
    assert!(agent.is_some());
    assert_eq!(agent.unwrap().name, "code-review");
}

#[test]
fn test_register_duplicate_returns_err() {
    let mut reg = AgentRegistry::new();
    reg.register(make_agent("test", AgentMode::Primary))
        .unwrap();
    let result = reg.register(make_agent("test", AgentMode::Primary));
    assert!(result.is_err());
}

#[test]
fn test_get_unknown_returns_none() {
    let reg = AgentRegistry::new();
    assert!(reg.get("nonexistent").is_none());
}

#[test]
fn test_default_prefers_named_default() {
    let mut reg = AgentRegistry::new();
    reg.register(make_agent("other", AgentMode::Primary))
        .unwrap();
    reg.register(make_agent("default", AgentMode::Primary))
        .unwrap();
    let def = reg.default().unwrap();
    assert_eq!(def.name, "default");
}

#[test]
fn test_default_falls_back_to_first_primary() {
    let mut reg = AgentRegistry::new();
    reg.register(make_agent("alpha", AgentMode::Primary))
        .unwrap();
    reg.register(make_agent("beta", AgentMode::Subagent))
        .unwrap();
    let def = reg.default().unwrap();
    assert_eq!(def.name, "alpha");
}

#[test]
fn test_default_returns_none_when_all_subagent() {
    let mut reg = AgentRegistry::new();
    reg.register(make_agent("s1", AgentMode::Subagent)).unwrap();
    reg.register(make_agent("s2", AgentMode::Subagent)).unwrap();
    assert!(reg.default().is_none());
}

#[test]
fn test_list_returns_all() {
    let mut reg = AgentRegistry::new();
    reg.register(make_agent("a", AgentMode::Primary)).unwrap();
    reg.register(make_agent("b", AgentMode::Subagent)).unwrap();
    assert_eq!(reg.list().len(), 2);
}

#[test]
fn test_list_subagents_only() {
    let mut reg = AgentRegistry::new();
    reg.register(make_agent("general", AgentMode::Primary))
        .unwrap();
    reg.register(make_agent("reviewer", AgentMode::Subagent))
        .unwrap();
    reg.register(make_agent("both", AgentMode::All)).unwrap();
    let subs = reg.list_subagents();
    assert_eq!(subs.len(), 2);
    assert!(
        subs.iter()
            .all(|a| a.name == "reviewer" || a.name == "both")
    );
}
