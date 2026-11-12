//! 验证者模块单元测试。

use super::*;

fn tool_context() -> ToolContext {
    ToolContext {
        working_dir: PathBuf::from("/tmp"),
        session_id: None,
        permission_rules: None,
        global_tx: None,
        visp_trace_id: None,
        iter_span_w3c_id: None,
    }
}

fn valid_args() -> serde_json::Value {
    serde_json::json!({
        "passed": true,
        "summary": "核心场景已实际验证通过",
        "evidence": [{
            "check": "10 万行 CSV 按列名查询",
            "method": "生成大文件并执行查询",
            "result": "返回正确行数"
        }]
    })
}

#[tokio::test]
async fn test_submit_tool_writes_verdict_cell() {
    let cell = VerdictCell::new();
    let tool = SubmitVerificationTool::new(cell.clone());

    let result = tool.execute(valid_args(), &tool_context()).await;
    assert!(!result.is_error, "合法参数应提交成功");

    let report = cell.take().expect("结论应写入共享槽");
    assert!(report.completed, "结构化提交表示验证者完成了验证");
    assert!(report.passed);
    assert_eq!(report.summary, "核心场景已实际验证通过");
    assert_eq!(report.evidence.len(), 1);
    assert_eq!(report.evidence[0].check, "10 万行 CSV 按列名查询");
    assert_eq!(report.evidence[0].method, "生成大文件并执行查询");
    assert_eq!(report.evidence[0].result, "返回正确行数");
}

#[tokio::test]
async fn test_submit_tool_rejects_missing_or_empty_evidence() {
    let cell = VerdictCell::new();
    let tool = SubmitVerificationTool::new(cell.clone());

    let missing = serde_json::json!({
        "passed": true,
        "summary": "没有证据"
    });
    let result = tool.execute(missing, &tool_context()).await;
    assert!(result.is_error, "缺少 evidence 必须报错");

    let empty = serde_json::json!({
        "passed": true,
        "summary": "空证据",
        "evidence": []
    });
    let result = tool.execute(empty, &tool_context()).await;
    assert!(result.is_error, "空 evidence 必须报错");

    let missing_field = serde_json::json!({
        "passed": true,
        "summary": "字段不全",
        "evidence": [{ "check": "只写了检查项" }]
    });
    let result = tool.execute(missing_field, &tool_context()).await;
    assert!(result.is_error, "证据缺少 method/result 必须报错");

    assert!(cell.take().is_none(), "校验失败不得写入结论");
}

#[tokio::test]
async fn test_submit_tool_only_accepts_first_verdict() {
    let cell = VerdictCell::new();
    let tool = SubmitVerificationTool::new(cell.clone());

    let first = tool.execute(valid_args(), &tool_context()).await;
    assert!(!first.is_error);

    let second_args = serde_json::json!({
        "passed": false,
        "summary": "第二次提交",
        "evidence": [{
            "check": "重复提交",
            "method": "再次调用",
            "result": "应被拒绝"
        }]
    });
    let second = tool.execute(second_args, &tool_context()).await;
    assert!(second.is_error, "重复提交必须报错");
    assert_eq!(second.content, "verification already submitted");

    let report = cell.take().expect("首次结论应保留");
    assert!(report.passed, "首次结果应生效");
    assert_eq!(report.summary, "核心场景已实际验证通过");
}

#[test]
fn test_build_verification_prompt_contains_request_paths_and_context() {
    let prompt = build_verification_prompt(
        "请实现 CSV 列名查询",
        &["src/a.rs".to_string(), "src/b.rs".to_string()],
        "diff --git a/src/a.rs b/src/a.rs",
    );

    assert!(prompt.contains("请实现 CSV 列名查询"));
    assert!(prompt.contains("src/a.rs"));
    assert!(prompt.contains("src/b.rs"));
    assert!(prompt.contains("diff --git a/src/a.rs b/src/a.rs"));
    assert!(prompt.contains("submit_verification"));
    assert!(prompt.contains("原始用户请求（唯一需求来源）"));
    assert!(prompt.contains("本轮改动的文件"));
}

#[test]
fn test_build_verification_prompt_without_git_info() {
    let prompt = build_verification_prompt("需求", &[], "");
    assert!(prompt.contains("(无 git 信息，请直接检查工作区)"));
    assert!(prompt.contains("(无)\n"));
}

#[test]
fn test_system_prompt_states_isolation_rules() {
    let prompt = build_verifier_system_prompt();
    assert!(prompt.contains("没有参与实现"));
    assert!(prompt.contains("不得相信实现者的测试"));
    assert!(prompt.contains("不能只重跑"));
    assert!(prompt.contains("submit_verification"));
    assert!(prompt.contains("实际执行"));
}

#[test]
fn test_is_mutating_tool() {
    for name in [
        "write_file",
        "edit_file",
        "multi_edit",
        "apply_patch",
        "notebook_edit",
        "WRITE_FILE",
        "Edit_File",
    ] {
        assert!(is_mutating_tool(name), "{name} 应被视为修改工具");
    }

    for name in ["read_file", "grep", "bash", "glob", ""] {
        assert!(!is_mutating_tool(name), "{name} 不应被视为修改工具");
    }
}

#[test]
fn test_extract_changed_path() {
    assert_eq!(
        extract_changed_path(r#"{"path":"src/lib.rs","content":"x"}"#),
        Some("src/lib.rs".to_string())
    );
    assert_eq!(
        extract_changed_path(r#"{"file_path":"src/main.rs"}"#),
        Some("src/main.rs".to_string())
    );
    assert_eq!(
        extract_changed_path(r#"{"path":"  src/trim.rs  "}"#),
        Some("src/trim.rs".to_string())
    );
    assert_eq!(extract_changed_path(r#"{"path":"not json"#), None);
    assert_eq!(extract_changed_path(r#"{"content":"x"}"#), None);
    assert_eq!(extract_changed_path(r#"{"path":""}"#), None);
}

#[test]
fn test_verifier_config_defaults() {
    let config = VerifierConfig::default();
    assert!(config.enabled);
    assert_eq!(config.max_rounds, 2);
    assert_eq!(config.model, None);
    assert_eq!(config.timeout_secs, 600);
    assert_eq!(config.max_steps, 40);

    assert!(!VerifierConfig::disabled().enabled);
}

#[test]
fn test_failure_report_has_no_evidence() {
    let report = VerificationReport::failure("验证者超时");
    assert!(!report.passed);
    assert!(
        !report.completed,
        "超时属于「验证没做成」，不应触发主 agent 返工"
    );
    assert_eq!(report.summary, "验证者超时");
    assert!(report.evidence.is_empty());
}
