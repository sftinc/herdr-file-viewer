//! Live refresh's pure pieces: which changed paths can change what the viewer shows (the event
//! filter), when a burst of changes has settled (the debounce), and where git keeps its state.

mod common;

use common::{TempDir, git, init_repo_with_commit};
use herdr_file_viewer::watch::{Debounce, EventFilter, MAX_WAIT, QUIET};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

fn canon(p: &Path) -> PathBuf {
    p.canonicalize().unwrap()
}

/// A root with a `.git` dir inside it, canonicalized (the OS watcher reports real paths).
fn repo_layout() -> (TempDir, PathBuf, PathBuf) {
    let dir = TempDir::new();
    fs::create_dir_all(dir.path().join(".git/info")).unwrap();
    let root = canon(dir.path());
    let git_dir = root.join(".git");
    (dir, root, git_dir)
}

#[test]
fn inside_a_git_dir_only_index_head_refs_and_logs_head_matter() {
    let (_dir, root, git_dir) = repo_layout();
    let filter = EventFilter::new(&root, std::slice::from_ref(&git_dir));
    for keep in [
        "index",
        "HEAD",
        "packed-refs",
        "logs/HEAD",
        "refs/heads/main",
        "refs/tags/v1",
    ] {
        assert!(
            filter.relevant(&git_dir.join(keep), false),
            "{keep} changes status or branch"
        );
    }
    for drop in [
        "index.lock",
        "objects/ab/cdef",
        "COMMIT_EDITMSG",
        "logs/refs/heads/main",
        "FETCH_HEAD",
    ] {
        assert!(
            !filter.relevant(&git_dir.join(drop), false),
            "{drop} is git's own bookkeeping"
        );
    }
}

#[test]
fn a_common_git_dir_outside_the_root_uses_the_same_allow_list() {
    let (_dir, root, git_dir) = repo_layout();
    let common_tmp = TempDir::new();
    let common = canon(common_tmp.path());
    let filter = EventFilter::new(&root, &[git_dir, common.clone()]);
    assert!(filter.relevant(&common.join("refs/heads/feature"), false));
    assert!(filter.relevant(&common.join("packed-refs"), false));
    assert!(!filter.relevant(&common.join("objects/12/3456"), false));
}

#[test]
fn a_path_outside_the_root_and_git_dirs_is_dropped() {
    let (_dir, root, git_dir) = repo_layout();
    let elsewhere = TempDir::new();
    let filter = EventFilter::new(&root, &[git_dir]);
    assert!(!filter.relevant(&canon(elsewhere.path()).join("x.rs"), false));
    assert!(filter.relevant(&root.join("src/main.rs"), false));
}

#[test]
fn root_gitignore_and_info_exclude_drop_matching_paths_unless_show_ignored() {
    let (_dir, root, git_dir) = repo_layout();
    fs::write(root.join(".gitignore"), "target/\n*.log\n").unwrap();
    fs::write(git_dir.join("info/exclude"), "secret.txt\n").unwrap();
    let filter = EventFilter::new(&root, &[git_dir]);
    assert!(!filter.relevant(&root.join("target/debug/app"), false));
    assert!(!filter.relevant(&root.join("build.log"), false));
    assert!(!filter.relevant(&root.join("secret.txt"), false));
    assert!(filter.relevant(&root.join("src/lib.rs"), false));
    assert!(
        filter.relevant(&root.join(".gitignore"), false),
        "editing the rules matters"
    );
    assert!(
        filter.relevant(&root.join("target/debug/app"), true),
        "show-ignored skips the filter"
    );
}

#[test]
fn reload_picks_up_an_edited_gitignore() {
    let (_dir, root, git_dir) = repo_layout();
    let mut filter = EventFilter::new(&root, &[git_dir]);
    assert!(filter.relevant(&root.join("a.log"), false));
    fs::write(root.join(".gitignore"), "*.log\n").unwrap();
    filter.reload();
    assert!(!filter.relevant(&root.join("a.log"), false));
}

#[test]
fn debounce_fires_after_a_quiet_gap_once_per_burst() {
    let t0 = Instant::now();
    let mut d = Debounce::default();
    assert!(!d.due(t0), "no events, nothing due");
    d.event(t0);
    assert!(!d.due(t0 + QUIET - Duration::from_millis(1)));
    assert!(d.due(t0 + QUIET));
    assert!(
        !d.due(t0 + QUIET + Duration::from_millis(50)),
        "reset after firing"
    );
}

#[test]
fn debounce_fires_at_max_wait_under_continuous_events() {
    let t0 = Instant::now();
    let mut d = Debounce::default();
    let mut t = t0;
    while t < t0 + MAX_WAIT {
        d.event(t);
        assert!(
            !d.due(t),
            "never quiet, and the burst is younger than MAX_WAIT"
        );
        t += Duration::from_millis(100);
    }
    d.event(t);
    assert!(
        d.due(t),
        "a burst that never goes quiet still refreshes at MAX_WAIT"
    );
}

#[test]
fn git_dirs_of_a_repo_a_linked_worktree_and_a_plain_dir() {
    let repo_tmp = TempDir::new();
    let repo = repo_tmp.path().join("repo");
    fs::create_dir_all(&repo).unwrap();
    init_repo_with_commit(&repo);
    let canon_all = |v: Vec<PathBuf>| v.into_iter().map(|p| canon(&p)).collect::<Vec<_>>();

    assert_eq!(
        canon_all(herdr_file_viewer::git::git_dirs(&repo)),
        vec![canon(&repo.join(".git"))]
    );

    let wt = repo_tmp.path().join("wt");
    git(
        &repo,
        &["worktree", "add", "-q", wt.to_str().unwrap(), "-b", "wt"],
    );
    assert_eq!(
        canon_all(herdr_file_viewer::git::git_dirs(&wt)),
        vec![
            canon(&repo.join(".git/worktrees/wt")),
            canon(&repo.join(".git"))
        ]
    );

    let plain = TempDir::new();
    assert!(herdr_file_viewer::git::git_dirs(plain.path()).is_empty());
}
