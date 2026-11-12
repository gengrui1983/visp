//! `[verifier]` 配置段测试：默认值、TOML 解析、项目级字段合并。

use super::*;

#[test]
fn test_verifier_section_defaults_are_explicit() {
    let config = DaemonConfig::default();
    assert!(config.verifier.is_enabled(), "独立验证默认开启");
    assert_eq!(
        config.verifier.effective_max_rounds(),
        2,
        "默认返工上限 2 轮"
    );
    assert_eq!(config.verifier.model, None, "默认继承主 agent 模型");
    assert_eq!(config.verifier.effective_timeout_secs(), 600);
    assert_eq!(config.verifier.effective_max_steps(), 40);
}

#[test]
fn test_verifier_section_parses_toml() {
    let raw = r#"
        [verifier]
        enabled = false
        max_rounds = 0
        model = "Mock/reviewer"
        timeout_secs = 120
        max_steps = 12
    "#;
    let config: DaemonConfig = toml::from_str(raw).unwrap();
    assert!(!config.verifier.is_enabled());
    assert_eq!(config.verifier.effective_max_rounds(), 0);
    assert_eq!(config.verifier.model.as_deref(), Some("Mock/reviewer"));
    assert_eq!(config.verifier.effective_timeout_secs(), 120);
    assert_eq!(config.verifier.effective_max_steps(), 12);
}

#[test]
fn test_verifier_section_omitted_fields_keep_global_values_in_merge() {
    let mut global = VerifierSection {
        enabled: Some(true),
        max_rounds: Some(3),
        model: Some("Mock/global-reviewer".into()),
        timeout_secs: Some(900),
        max_steps: Some(50),
    };
    let project = VerifierSection {
        max_rounds: Some(1),
        ..Default::default()
    };
    merge_verifier_sections(&mut global, &project);
    assert!(global.is_enabled(), "未在项目配置中出现的字段保持全局值");
    assert_eq!(global.effective_max_rounds(), 1, "项目显式字段覆盖全局");
    assert_eq!(
        global.model.as_deref(),
        Some("Mock/global-reviewer"),
        "项目未写 model 时保留全局 model"
    );
    assert_eq!(global.effective_timeout_secs(), 900);
    assert_eq!(global.effective_max_steps(), 50);
}

#[test]
fn test_verifier_section_serializes_empty_as_nothing() {
    let config = DaemonConfig::default();
    let text = toml::to_string(&config).unwrap();
    assert!(
        !text.contains("[verifier]"),
        "默认配置不应因空 verifier 段污染 daemon.toml: {text}"
    );
}
