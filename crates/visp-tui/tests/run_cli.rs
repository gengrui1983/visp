//! End-to-end tests for `visp-tui run`.
//!
//! A real `visp-tui run` process is started against a fake ACP agent (a small
//! Python script) via the `VISP_ACP_BIN` test hook. No daemon and no network
//! are involved.
#![cfg(unix)]

use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use tempfile::TempDir;

const FAKE_AGENT: &str = r#"#!/usr/bin/env python3
import json
import os
import sys

SCENARIO = os.environ.get("FAKE_ACP_SCENARIO", "ok")
ECHO = os.environ.get("FAKE_ACP_ECHO_PROMPT") == "1"
SID = "fake-session"


def send(obj):
    sys.stdout.write(json.dumps(obj, separators=(",", ":")) + "\n")
    sys.stdout.flush()


def update(update_obj):
    send({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {"sessionId": SID, "update": update_obj},
    })


def text(value):
    update({
        "sessionUpdate": "agent_message_chunk",
        "content": {"type": "text", "text": value},
    })


def read_message():
    line = sys.stdin.readline()
    if not line:
        return None
    return json.loads(line)


def read_response(mid):
    while True:
        msg = read_message()
        if msg is None:
            return None
        if msg.get("id") == mid and "method" not in msg:
            return msg


def prompt_text(params):
    parts = []
    for block in params.get("prompt", []):
        if block.get("type") == "text":
            parts.append(block.get("text", ""))
    return "".join(parts)


def main():
    argv_file = os.environ.get("FAKE_ACP_ARGV_FILE")
    if argv_file:
        with open(argv_file, "w") as f:
            json.dump(sys.argv[1:], f)
    while True:
        msg = read_message()
        if msg is None:
            return
        method = msg.get("method")
        mid = msg.get("id")

        if method == "initialize":
            send({
                "jsonrpc": "2.0",
                "id": mid,
                "result": {
                    "protocolVersion": 1,
                    "agentCapabilities": {},
                    "authMethods": [],
                },
            })
        elif method == "session/new":
            if SCENARIO == "drop_after_new":
                return
            send({"jsonrpc": "2.0", "id": mid, "result": {"sessionId": SID}})
        elif method == "session/prompt":
            if SCENARIO == "drop_in_prompt":
                return
            if SCENARIO == "cancel":
                text("Working")
                while True:
                    cancel = read_message()
                    if cancel is None:
                        return
                    if cancel.get("method") == "session/cancel":
                        send({"jsonrpc": "2.0", "id": mid,
                              "result": {"stopReason": "cancelled"}})
                        return
            if SCENARIO == "error":
                send({"jsonrpc": "2.0", "id": mid, "result": {"stopReason": "refusal"}})
                continue
            if ECHO:
                text("PROMPT=" + prompt_text(msg.get("params", {})) + "\n")
            text("Hello ")
            update({
                "sessionUpdate": "tool_call",
                "toolCallId": "c1",
                "title": "bash(ls)",
                "kind": "execute",
                "status": "pending",
            })
            send({
                "jsonrpc": "2.0",
                "id": 900,
                "method": "session/request_permission",
                "params": {
                    "sessionId": SID,
                    "toolCall": {"toolCallId": "c1", "title": "Allow tool: bash(ls)?"},
                    "options": [
                        {"optionId": "allow_once", "name": "Allow", "kind": "allow_once"},
                        {"optionId": "reject_once", "name": "Reject", "kind": "reject_once"},
                    ],
                },
            })
            response = read_response(900) or {}
            outcome = ((response.get("result") or {}).get("outcome") or {})
            if outcome.get("outcome") == "selected" and outcome.get("optionId") == "allow_once":
                text("ALLOWED")
            elif outcome.get("outcome") == "selected" and outcome.get("optionId") == "reject_once":
                text("DENIED")
            else:
                text("CANCELLED")
            update({
                "sessionUpdate": "tool_call_update",
                "toolCallId": "c1",
                "status": "failed",
                "content": [
                    {"type": "content", "content": {"type": "text", "text": "boom"}}
                ],
            })
            text(" world")
            send({"jsonrpc": "2.0", "id": mid, "result": {"stopReason": "end_turn"}})
        elif method == "session/cancel":
            pass
        elif mid is not None:
            send({
                "jsonrpc": "2.0",
                "id": mid,
                "error": {"code": -32601, "message": "method not found"},
            })


main()
"#;

fn fake_agent(dir: &Path) -> PathBuf {
    let path = dir.join("fake-acp-agent.py");
    std::fs::write(&path, FAKE_AGENT).unwrap();
    let mut perms = std::fs::metadata(&path).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&path, perms).unwrap();
    path
}

struct Fixture {
    _dir: TempDir,
    agent: PathBuf,
    project: PathBuf,
    config: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let dir = TempDir::new().unwrap();
        let agent = fake_agent(dir.path());
        let project = dir.path().join("project");
        let config = dir.path().join("cfg");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::create_dir_all(&config).unwrap();
        Self {
            agent,
            project,
            config,
            _dir: dir,
        }
    }

    fn run(&self, args: &[&str], stdin: Option<&str>, env: &[(&str, &str)]) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_visp-tui"));
        command
            .args(args)
            .env("VISP_ACP_BIN", &self.agent)
            .env("VISP_CONFIG_DIR", &self.config)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (key, value) in env {
            command.env(key, value);
        }
        if stdin.is_some() {
            command.stdin(Stdio::piped());
        } else {
            command.stdin(Stdio::null());
        }
        let mut child = command.spawn().unwrap();
        if let Some(input) = stdin {
            child
                .stdin
                .take()
                .unwrap()
                .write_all(input.as_bytes())
                .unwrap();
        }
        child.wait_with_output().unwrap()
    }

    fn run_opts(&self, scenario: &str, extra: &[&str], prompt: &[&str]) -> Output {
        let mut args = vec![
            "run",
            "-p",
            self.project.to_str().unwrap(),
            "--config-dir",
            self.config.to_str().unwrap(),
        ];
        args.extend_from_slice(extra);
        args.extend_from_slice(prompt);
        self.run(&args, None, &[("FAKE_ACP_SCENARIO", scenario)])
    }
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn one_prompt_success_with_denied_approval() {
    let fx = Fixture::new();
    let output = fx.run_opts("ok", &[], &["hello", "world"]);
    assert_eq!(output.status.code(), Some(0), "stderr: {}", stderr(&output));
    assert_eq!(stdout(&output), "Hello DENIED world\n");
    let err = stderr(&output);
    assert!(err.contains("[tool #1] bash(ls)"), "stderr: {err}");
    assert!(err.contains("[tool error] bash(ls): boom"), "stderr: {err}");
    assert!(err.contains("use --yes"), "stderr: {err}");
    assert!(err.contains("1 tool calls"), "stderr: {err}");
}

#[test]
fn yes_approves_tool_once() {
    let fx = Fixture::new();
    let output = fx.run_opts("ok", &["-y"], &["hello"]);
    assert_eq!(output.status.code(), Some(0), "stderr: {}", stderr(&output));
    assert_eq!(stdout(&output), "Hello ALLOWED world\n");
    let err = stderr(&output);
    assert!(err.contains("1 granted / 0 denied"), "stderr: {err}");
    assert!(!err.contains("denied (use --yes"), "stderr: {err}");
}

#[test]
fn prompt_words_are_joined_and_stdin_dash_is_read() {
    let fx = Fixture::new();
    let output = fx.run(
        &[
            "run",
            "-p",
            fx.project.to_str().unwrap(),
            "--config-dir",
            fx.config.to_str().unwrap(),
            "fix",
            "the bug",
        ],
        None,
        &[("FAKE_ACP_ECHO_PROMPT", "1")],
    );
    assert_eq!(output.status.code(), Some(0), "stderr: {}", stderr(&output));
    assert!(
        stdout(&output).contains("PROMPT=fix the bug"),
        "stdout: {}",
        stdout(&output)
    );

    let output = fx.run(
        &[
            "run",
            "-p",
            fx.project.to_str().unwrap(),
            "--config-dir",
            fx.config.to_str().unwrap(),
            "-",
        ],
        Some("from stdin"),
        &[("FAKE_ACP_ECHO_PROMPT", "1")],
    );
    assert_eq!(output.status.code(), Some(0), "stderr: {}", stderr(&output));
    assert!(
        stdout(&output).contains("PROMPT=from stdin"),
        "stdout: {}",
        stdout(&output)
    );
}

#[test]
fn empty_prompt_is_a_usage_error() {
    let fx = Fixture::new();
    let output = fx.run(&["run"], None, &[]);
    assert_eq!(output.status.code(), Some(2));
    assert!(
        stderr(&output).contains("empty prompt"),
        "stderr: {}",
        stderr(&output)
    );
}

#[test]
fn clap_errors_are_usage_errors() {
    let fx = Fixture::new();
    let output = fx.run(&["run", "--definitely-not-a-flag"], None, &[]);
    assert_eq!(output.status.code(), Some(2));
}

#[test]
fn agent_error_exits_1() {
    let fx = Fixture::new();
    let output = fx.run_opts("error", &[], &["hello"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr(&output).contains("refused"),
        "stderr: {}",
        stderr(&output)
    );
}

#[test]
fn connection_loss_exits_3() {
    let fx = Fixture::new();
    let output = fx.run_opts("drop_after_new", &[], &["hello"]);
    assert_eq!(
        output.status.code(),
        Some(3),
        "stdout: {} stderr: {}",
        stdout(&output),
        stderr(&output)
    );
    assert!(
        stderr(&output).contains("connection"),
        "stderr: {}",
        stderr(&output)
    );
}

#[test]
fn flags_are_forwarded_to_the_acp_agent() {
    let fx = Fixture::new();
    let argv_file = fx._dir.path().join("argv.json");
    let output = fx.run(
        &[
            "run",
            "-p",
            fx.project.to_str().unwrap(),
            "--config-dir",
            fx.config.to_str().unwrap(),
            "--model",
            "provider/model",
            "--temperature",
            "0.2",
            "--thinking-budget",
            "2048",
            "hello",
        ],
        None,
        &[("FAKE_ACP_ARGV_FILE", argv_file.to_str().unwrap())],
    );
    assert_eq!(output.status.code(), Some(0), "stderr: {}", stderr(&output));
    let argv: Vec<String> =
        serde_json::from_str(&std::fs::read_to_string(&argv_file).unwrap()).unwrap();
    let has_pair = |a: &str, b: &str| argv.windows(2).any(|w| w == [a, b]);
    assert!(
        has_pair("--project", fx.project.to_str().unwrap()),
        "{argv:?}"
    );
    assert!(
        has_pair("--config-dir", fx.config.to_str().unwrap()),
        "{argv:?}"
    );
    assert!(has_pair("--model", "provider/model"), "{argv:?}");
    assert!(has_pair("--temperature", "0.2"), "{argv:?}");
    assert!(has_pair("--thinking-budget", "2048"), "{argv:?}");
}

#[test]
fn connection_loss_while_prompting_exits_3() {
    let fx = Fixture::new();
    let output = fx.run_opts("drop_in_prompt", &[], &["hello"]);
    assert_eq!(
        output.status.code(),
        Some(3),
        "stdout: {} stderr: {}",
        stdout(&output),
        stderr(&output)
    );
    assert!(
        stderr(&output).contains("connection"),
        "stderr: {}",
        stderr(&output)
    );
}

#[test]
fn ctrl_c_sends_cancel_and_exits_130() {
    use std::time::Duration;

    let fx = Fixture::new();
    let child = std::process::Command::new(env!("CARGO_BIN_EXE_visp-tui"))
        .args([
            "run",
            "-p",
            fx.project.to_str().unwrap(),
            "--config-dir",
            fx.config.to_str().unwrap(),
            "hello",
        ])
        .env("VISP_ACP_BIN", &fx.agent)
        .env("VISP_CONFIG_DIR", &fx.config)
        .env("FAKE_ACP_SCENARIO", "cancel")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_millis(300));
    let status = std::process::Command::new("kill")
        .args(["-INT", &child.id().to_string()])
        .status()
        .unwrap();
    assert!(status.success(), "failed to send SIGINT");
    let output = child.wait_with_output().unwrap();
    assert_eq!(
        output.status.code(),
        Some(130),
        "stdout: {} stderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let err = String::from_utf8_lossy(&output.stderr);
    assert!(err.contains("interrupted"), "stderr: {err}");
}
