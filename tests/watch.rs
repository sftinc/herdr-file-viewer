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

#[test]
fn the_live_watcher_reports_a_created_file() {
    use herdr_file_viewer::watch::{NotifyWatch, WatchEvent, WatchService};
    let dir = TempDir::new();
    let root = canon(dir.path());
    let handle = NotifyWatch
        .watch(&root, &[], false)
        .expect("the watcher starts");
    fs::write(root.join("new.txt"), "x").unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match handle.rx.recv_timeout(left) {
            Ok(WatchEvent::Paths(paths)) if paths.iter().any(|p| p.ends_with("new.txt")) => break,
            Ok(_) => continue,
            Err(e) => panic!("no event for new.txt: {e}"),
        }
    }
}

/// `rel`'s `/`-separated parts joined onto `root` one by one, so the path uses the native
/// separator (a `"a/b"` join on a canonical Windows root is a different path).
fn at(root: &Path, rel: &str) -> PathBuf {
    rel.split('/')
        .filter(|c| !c.is_empty())
        .fold(root.to_path_buf(), |p, c| p.join(c))
}

/// A repo whose tree shows `src/nested` and hides `node_modules/` and `target/`.
fn repo_with_ignored_dirs() -> (TempDir, PathBuf, Vec<PathBuf>) {
    let dir = TempDir::new();
    let root = canon(dir.path());
    init_repo_with_commit(&root);
    fs::write(at(&root, ".gitignore"), "node_modules/\ntarget/\n").unwrap();
    for d in ["node_modules/a/b", "target/x", "src/nested"] {
        fs::create_dir_all(at(&root, d)).unwrap();
    }
    let git_dirs = herdr_file_viewer::git::git_dirs(&root)
        .iter()
        .map(|d| canon(d))
        .collect();
    (dir, root, git_dirs)
}

#[test]
fn only_the_folders_the_tree_shows_and_the_git_state_are_watched() {
    use herdr_file_viewer::watch::watch_dirs;
    let (_dir, root, git_dirs) = repo_with_ignored_dirs();
    let dirs = watch_dirs(&root, &git_dirs, true);
    for present in [
        "",
        "src",
        "src/nested",
        ".git",
        ".git/refs",
        ".git/refs/heads",
    ] {
        assert!(dirs.contains(&at(&root, present)), "{present:?} is watched");
    }
    for absent in [
        "node_modules",
        "node_modules/a/b",
        "target",
        "target/x",
        ".git/objects",
        ".git/info",
    ] {
        assert!(
            !dirs.contains(&at(&root, absent)),
            "{absent:?} is not watched"
        );
    }
}

#[test]
fn a_new_folder_is_watched_with_its_visible_subfolders_unless_ignored() {
    use herdr_file_viewer::watch::new_dir_watch_dirs;
    let (_dir, root, git_dirs) = repo_with_ignored_dirs();
    fs::create_dir_all(at(&root, "lib/inner")).unwrap();
    fs::create_dir_all(at(&root, "lib/target/deep")).unwrap();
    fs::create_dir_all(at(&root, ".git/refs/heads/feat/x")).unwrap();
    assert_eq!(
        new_dir_watch_dirs(&at(&root, "lib"), &root, &git_dirs, true),
        vec![at(&root, "lib"), at(&root, "lib/inner")]
    );
    assert!(
        new_dir_watch_dirs(&at(&root, "lib/target"), &root, &git_dirs, true).is_empty(),
        "an ignored new folder is not watched"
    );
    assert_eq!(
        new_dir_watch_dirs(&at(&root, ".git/refs/heads/feat"), &root, &git_dirs, true),
        vec![
            at(&root, ".git/refs/heads/feat"),
            at(&root, ".git/refs/heads/feat/x")
        ]
    );
    assert!(new_dir_watch_dirs(&at(&root, ".git/objects"), &root, &git_dirs, true).is_empty());
}

#[test]
fn the_watch_budget_is_a_quarter_of_the_limit() {
    use herdr_file_viewer::watch::within_budget;
    assert!(within_budget(2048, Some(8192)), "exactly a quarter passes");
    assert!(!within_budget(2049, Some(8192)), "one over fails");
    assert!(
        within_budget(1_000_000, None),
        "an unreadable limit never refuses"
    );
}

/// Wait (bounded) for the first event naming a path that ends with `name`, returning every event
/// that arrived before it.
#[cfg(target_os = "linux")]
fn events_until(
    handle: &herdr_file_viewer::watch::WatchHandle,
    name: &str,
    rescan_counts: bool,
) -> Vec<herdr_file_viewer::watch::WatchEvent> {
    use herdr_file_viewer::watch::WatchEvent;
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut before = Vec::new();
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match handle.rx.recv_timeout(left) {
            Ok(WatchEvent::Paths(paths)) if paths.iter().any(|p| p.ends_with(name)) => {
                return before;
            }
            Ok(WatchEvent::Rescan) if rescan_counts => return before,
            Ok(ev) => before.push(ev),
            Err(e) => panic!("no event for {name}: {e}"),
        }
    }
}

#[cfg(target_os = "linux")]
#[test]
fn linux_a_file_in_a_folder_created_after_start_is_reported() {
    use herdr_file_viewer::watch::{NotifyWatch, WatchService};
    let (_dir, root, git_dirs) = repo_with_ignored_dirs();
    let handle = NotifyWatch
        .watch(&root, &git_dirs, true)
        .expect("the watcher starts");
    fs::create_dir(root.join("fresh")).unwrap();
    fs::write(root.join("fresh/new.txt"), "x").unwrap();
    // Either the file's own event (its folder's watch landed first) or the Rescan sent after the
    // watch was added (which covers a file created in the gap).
    events_until(&handle, "new.txt", true);
}

#[cfg(target_os = "linux")]
#[test]
fn linux_a_file_under_an_ignored_folder_produces_no_event() {
    use herdr_file_viewer::watch::{NotifyWatch, WatchEvent, WatchService};
    let (_dir, root, git_dirs) = repo_with_ignored_dirs();
    let handle = NotifyWatch
        .watch(&root, &git_dirs, true)
        .expect("the watcher starts");
    // Proven by order, not by sleeping: inotify's queue is FIFO per instance, so an event for the
    // ignored file would arrive before the marker's.
    fs::write(root.join("target/x/ignored.o"), "x").unwrap();
    fs::write(root.join("src/marker.txt"), "x").unwrap();
    let before = events_until(&handle, "marker.txt", false);
    for ev in before {
        if let WatchEvent::Paths(paths) = ev {
            assert!(
                !paths.iter().any(|p| p.ends_with("ignored.o")),
                "an ignored folder is not watched: {paths:?}"
            );
        }
    }
}
