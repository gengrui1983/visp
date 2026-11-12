//! `visp run ...` is a pure process starter: arguments go verbatim to
//! `visp-tui run ...` and its exit code is passed through.
#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::process::{Command, Stdio};

#[test]
fn visp_run_forwards_every_argument_and_the_exit_code() {
    let dir = tempfile::tempdir().unwrap();
    let fake = dir.path().join("fake-visp-tui.sh");
    let out = dir.path().join("args.txt");
    std::fs::write(
        &fake,
        "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"$FAKE_TUI_ARGS\"\nexit 42\n",
    )
    .unwrap();
    let mut perms = std::fs::metadata(&fake).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&fake, perms).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_visp"))
        .args([
            "run",
            "-y",
            "--config-dir",
            "/cfg",
            "--model",
            "provider/model",
            "fix",
            "the bug",
        ])
        .env("VISP_TUI_BIN", &fake)
        .env("FAKE_TUI_ARGS", &out)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(42));
    let forwarded = std::fs::read_to_string(&out).unwrap();
    assert_eq!(
        forwarded,
        "run\n-y\n--config-dir\n/cfg\n--model\nprovider/model\nfix\nthe bug\n"
    );
}
