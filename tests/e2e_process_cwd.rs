//! e2e (pty): the viewer process sits in the viewed root, not in the directory herdr launched it
//! from. herdr runs the manifest's relative pane command from the plugin checkout, and its default
//! `new_cwd = "follow"` opens a new pane in the focused pane's live process cwd — so a viewer that
//! stayed put made every pane opened from it start in the plugin folder. The tell is the viewer's
//! own process cwd, read from outside (`/proc/<pid>/cwd` on Linux, `lsof` on macOS).
//!
//! Unix-only, like the rest of the `expectrl` pty e2e suite (see `tests/cli_smoke.rs`).
#![cfg(unix)]

mod common;

use common::{TempDir, canon, git, init_repo_with_commit, viewer_command};
use expectrl::{Expect, Session};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// The live cwd of process `pid`.
fn process_cwd(pid: i32) -> PathBuf {
    if cfg!(target_os = "linux") {
        return std::fs::read_link(format!("/proc/{pid}/cwd")).expect("read /proc cwd");
    }
    // `-Fn` prints one `n<path>` line for the cwd descriptor selected by `-a -d cwd`.
    let out = std::process::Command::new("lsof")
        .args(["-a", "-p", &pid.to_string(), "-d", "cwd", "-Fn"])
        .output()
        .expect("run lsof");
    let text = String::from_utf8_lossy(&out.stdout);
    let line = text
        .lines()
        .find_map(|l| l.strip_prefix('n'))
        .unwrap_or_else(|| panic!("no cwd in lsof output: {text:?}"));
    PathBuf::from(line)
}

fn context_json(focused: &Path) -> String {
    serde_json::json!({ "focused_pane_cwd": focused }).to_string()
}

#[test]
fn the_viewer_process_moves_into_the_root_from_the_launch_context() {
    let plugin_dir = TempDir::new();
    let project = TempDir::new();
    std::fs::write(project.path().join("seed.txt"), "x\n").unwrap();

    // Launched from the "plugin checkout", with herdr's context naming the project.
    let mut cmd = viewer_command(plugin_dir.path());
    cmd.env("HERDR_PLUGIN_CONTEXT_JSON", context_json(project.path()));
    let mut s = Session::spawn(cmd).expect("spawn the viewer in a pty");
    s.set_expect_timeout(Some(Duration::from_secs(15)));
    s.expect("seed.txt").expect("tree should list the project");

    // The cwd change happens before the first frame, so seeing the tree means it is done.
    let pid = s.get_process().pid().as_raw();
    assert_eq!(canon(&process_cwd(pid)), canon(project.path()));
    let _ = s
        .get_process_mut()
        .kill(expectrl::process::unix::Signal::SIGKILL);
}

#[test]
fn the_viewer_process_follows_a_worktree_switch() {
    let repo = TempDir::new();
    let main = repo.path();
    init_repo_with_commit(main);
    let feature = TempDir::new();
    let feature_path = feature.path().join("wt");
    git(
        main,
        &[
            "worktree",
            "add",
            "-b",
            "feature",
            feature_path.to_str().unwrap(),
        ],
    );
    std::fs::write(feature_path.join("FEATONLY.txt"), "FEATMARK\n").unwrap();

    let mut s = Session::spawn(viewer_command(main)).expect("spawn the viewer in a pty");
    s.set_expect_timeout(Some(Duration::from_secs(15)));
    s.expect("seed.txt").expect("tree should list main's files");

    // W → j → Enter re-roots to the feature worktree; opening its only-there file proves the
    // switch landed, and the cwd follows in the same loop turn as the switch.
    s.send("W").expect("open the picker");
    s.expect("Switch worktree").expect("picker renders");
    s.send("j").expect("move to the feature worktree");
    s.send("\r").expect("confirm the switch");
    s.send("\r").expect("open the feature file");
    s.expect("FEATMARK").expect("the feature worktree is shown");

    let pid = s.get_process().pid().as_raw();
    assert_eq!(canon(&process_cwd(pid)), canon(&feature_path));
    let _ = s
        .get_process_mut()
        .kill(expectrl::process::unix::Signal::SIGKILL);
    let _ = std::process::Command::new("git")
        .arg("-C")
        .arg(main)
        .args([
            "worktree",
            "remove",
            "--force",
            feature_path.to_str().unwrap(),
        ])
        .output();
}
