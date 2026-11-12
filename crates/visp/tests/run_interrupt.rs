//! Ctrl-C is delivered by the terminal to *every* process in the foreground
//! process group — the launcher and `visp-tui` alike. The launcher must not die
//! on that signal: it has to keep waiting for `visp-tui` to finish its own
//! shutdown and only then pass its exit code through.
//!
//! The fake `visp-tui` below catches SIGINT, takes a while to shut down, and
//! only then records that it is done. If the launcher exits early the marker is
//! missing when `wait()` returns.
#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// How long the fake `visp-tui` needs to shut down after SIGINT.
const CHILD_SHUTDOWN: Duration = Duration::from_millis(400);

#[test]
fn launcher_keeps_waiting_for_visp_tui_shutdown_after_sigint() {
    let dir = tempfile::tempdir().unwrap();
    let fake = dir.path().join("slow-visp-tui.sh");
    let ready = dir.path().join("child-ready");
    let done = dir.path().join("child-done");

    // `trap` only works if SIGINT still has its default disposition: a signal
    // ignored by the launcher *before* spawning would be inherited as SIG_IGN
    // across exec and this child could never observe it.
    std::fs::write(
        &fake,
        format!(
            "#!/bin/sh\n\
             : > \"{}\"\n\
             trap 'sleep {}; : > \"{}\"; exit 130' INT\n\
             while :; do sleep 0.02; done\n",
            ready.display(),
            CHILD_SHUTDOWN.as_secs_f64(),
            done.display(),
        ),
    )
    .unwrap();
    let mut perms = std::fs::metadata(&fake).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&fake, perms).unwrap();

    // Own process group: `process_group(0)` makes the launcher a group leader
    // and the fake visp-tui inherits that group, exactly like a shell job in
    // the foreground. `kill(-pgid, SIGINT)` is what Ctrl-C does.
    let mut launcher = Command::new(env!("CARGO_BIN_EXE_visp"))
        .args(["run", "-y", "a very long task"])
        .env("VISP_TUI_BIN", &fake)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .spawn()
        .unwrap();
    let pgid = launcher.id() as libc::pid_t;

    let deadline = Instant::now() + Duration::from_secs(10);
    while !ready.exists() {
        assert!(Instant::now() < deadline, "fake visp-tui never started");
        std::thread::sleep(Duration::from_millis(10));
    }

    let sent = Instant::now();
    // SAFETY: plain libc call, no memory safety concerns.
    unsafe { libc::kill(-pgid, libc::SIGINT) };

    let status = loop {
        if let Some(status) = launcher.try_wait().unwrap() {
            break status;
        }
        if Instant::now() > deadline {
            // SAFETY: see above.
            unsafe { libc::kill(-pgid, libc::SIGKILL) };
            panic!("launcher did not exit within 10s of SIGINT");
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let waited = sent.elapsed();
    // Reap anything the fake child may have left behind before asserting.
    // SAFETY: see above; group members are already gone if the fix works.
    unsafe { libc::kill(-pgid, libc::SIGKILL) };

    // The child exits 130; the launcher passes that through.
    assert_eq!(status.code(), Some(130), "launcher exit status");
    assert!(
        done.exists(),
        "launcher returned {waited:?} after SIGINT but the child had not \
         finished shutting down (marker missing)"
    );
    assert!(
        waited >= CHILD_SHUTDOWN,
        "launcher returned after {waited:?}, before the child's shutdown delay"
    );
}
