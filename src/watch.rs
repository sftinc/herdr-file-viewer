//! Live refresh's inputs: the OS file watcher behind a trait, the filter that decides which
//! changed paths can change what the viewer shows, and the debounce that decides when a burst of
//! changes has settled. Everything but the live watcher is pure; the controller drains the watcher
//! every tick and runs the status job off the UI thread.

use ignore::gitignore::{Gitignore, GitignoreBuilder};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// One batch from the watcher.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WatchEvent {
    /// These paths changed.
    Paths(Vec<PathBuf>),
    /// The OS dropped events, so anything may have changed.
    Rescan,
}

/// A running watcher: its events, and whatever keeps the OS watch alive (dropped with it).
/// `Send`, because it is built on the setup thread and handed to the UI thread.
pub struct WatchHandle {
    pub rx: mpsc::Receiver<WatchEvent>,
    pub guard: Box<dyn std::any::Any + Send>,
}

/// Starts OS file watches. Behind a trait so tests feed events without a real watcher. `Send +
/// Sync`, because setup runs off the UI thread: on Linux it walks the root to add its watches.
pub trait WatchService: Send + Sync {
    /// Watch the canonical `root` and its canonical git dirs (all of them, a `.git` inside the
    /// root included). `None` when the watcher cannot start (for example past the Linux watch
    /// budget); the viewer then refreshes on focus and `r` only.
    fn watch(&self, root: &Path, git_dirs: &[PathBuf], is_git_repo: bool) -> Option<WatchHandle>;
}

/// Decides whether a changed path can change what the viewer shows.
pub struct EventFilter {
    root: PathBuf,
    git_dirs: Vec<PathBuf>,
    ignore: Gitignore,
}

impl EventFilter {
    /// `root` and `git_dirs` are canonicalized, because the OS watcher reports real paths. The
    /// ignore rules are the root `.gitignore` and each git dir's `info/exclude`: what the tree
    /// hides. Global excludes are left out because the tree shows those files
    /// (`index::walk_builder` sets `git_global(false)`). Nested `.gitignore` files are not read
    /// either: a path one of them ignores costs an extra refresh, and a path one re-includes
    /// with `!pattern` waits for focus or `r`.
    pub fn new(root: &Path, git_dirs: &[PathBuf]) -> Self {
        let root = canonical(root);
        let git_dirs: Vec<PathBuf> = git_dirs.iter().map(|d| canonical(d)).collect();
        let mut builder = GitignoreBuilder::new(&root);
        // A missing or unreadable file only means no rules from it.
        let _ = builder.add(root.join(".gitignore"));
        for dir in &git_dirs {
            let _ = builder.add(dir.join("info").join("exclude"));
        }
        let ignore = builder.build().unwrap_or_else(|_| Gitignore::empty());
        Self {
            root,
            git_dirs,
            ignore,
        }
    }

    /// Re-read the ignore files, so a `.gitignore` edited mid-session applies to later events.
    pub fn reload(&mut self) {
        *self = Self::new(&self.root.clone(), &self.git_dirs.clone());
    }

    /// The canonical root.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The canonical git dir and common git dir (empty outside a repo).
    pub fn git_dirs(&self) -> &[PathBuf] {
        &self.git_dirs
    }

    /// Whether a change at `path` can change the tree, the git markers or the preview.
    pub fn relevant(&self, path: &Path, show_ignored: bool) -> bool {
        for dir in &self.git_dirs {
            if let Ok(rel) = path.strip_prefix(dir) {
                return git_dir_entry_matters(rel);
            }
        }
        let Ok(rel) = path.strip_prefix(&self.root) else {
            return false;
        };
        if show_ignored || rel.as_os_str().is_empty() {
            return true;
        }
        // Root-relative on purpose: `matched_path_or_any_parents` panics on a path outside the
        // matcher's root, and a relative path can never be.
        !self
            .ignore
            .matched_path_or_any_parents(rel, path.is_dir())
            .is_ignore()
    }
}

/// Inside a git dir, only these change status, the changed-set or the branch.
fn git_dir_entry_matters(rel: &Path) -> bool {
    rel == Path::new("index")
        || rel == Path::new("HEAD")
        || rel == Path::new("packed-refs")
        || rel == Path::new("logs/HEAD")
        || rel.starts_with("refs")
}

fn canonical(p: &Path) -> PathBuf {
    p.canonicalize().unwrap_or_else(|_| p.to_path_buf())
}

/// Start a refresh once no relevant event has arrived for this long…
pub const QUIET: Duration = Duration::from_millis(250);
/// …or once this long has passed since the first event of a burst, whichever comes first. This
/// limits how often refreshes start, not how soon the screen updates.
pub const MAX_WAIT: Duration = Duration::from_secs(1);

/// The burst being debounced. Plain `Instant`s in, so tests drive it without sleeping.
#[derive(Debug, Default)]
pub struct Debounce {
    first: Option<Instant>,
    last: Option<Instant>,
}

impl Debounce {
    /// A relevant event arrived at `now`.
    pub fn event(&mut self, now: Instant) {
        self.first.get_or_insert(now);
        self.last = Some(now);
    }

    /// Whether a refresh is due at `now`: true once per burst, after which the burst is cleared.
    pub fn due(&mut self, now: Instant) -> bool {
        let (Some(first), Some(last)) = (self.first, self.last) else {
            return false;
        };
        let due = now.saturating_duration_since(last) >= QUIET
            || now.saturating_duration_since(first) >= MAX_WAIT;
        if due {
            *self = Self::default();
        }
        due
    }
}

/// The folders Linux watches (each non-recursively, as inotify needs one watch per folder): every
/// folder the tree can show, walked with the tree's own rules, plus the parts of each git dir the
/// event filter allows (the dir itself, `logs/` and every folder under `refs/`). Ignored folders
/// such as `node_modules/` and `target/` are left out, which is most of a big repo's folders.
pub fn watch_dirs(root: &Path, git_dirs: &[PathBuf], is_git_repo: bool) -> Vec<PathBuf> {
    let mut dirs = visible_dirs(root, root, is_git_repo);
    for git_dir in git_dirs {
        dirs.push(git_dir.clone());
        let logs = git_dir.join("logs");
        if logs.is_dir() {
            dirs.push(logs);
        }
        dirs.extend(all_dirs(&git_dir.join("refs")));
    }
    dirs.sort();
    dirs.dedup();
    dirs
}

/// The folders to add when `dir` appears under a watched folder: it and its visible subfolders,
/// by the same rules as [`watch_dirs`]. Empty when the tree would not show it.
pub fn new_dir_watch_dirs(
    dir: &Path,
    root: &Path,
    git_dirs: &[PathBuf],
    is_git_repo: bool,
) -> Vec<PathBuf> {
    if !dir.symlink_metadata().is_ok_and(|m| m.is_dir()) {
        return Vec::new();
    }
    for git_dir in git_dirs {
        if let Ok(rel) = dir.strip_prefix(git_dir) {
            return if rel == Path::new("logs") {
                vec![dir.to_path_buf()]
            } else if rel.starts_with("refs") {
                all_dirs(dir)
            } else {
                Vec::new()
            };
        }
    }
    match dir.parent() {
        Some(parent) if dir.starts_with(root) && dir != root => {
            visible_dirs(parent, dir, is_git_repo)
        }
        _ => Vec::new(),
    }
}

/// The folders at or under `under` that the tree shows, walking from `from` (`under` or its
/// parent) so the rules decide whether `under` itself is shown.
fn visible_dirs(from: &Path, under: &Path, is_git_repo: bool) -> Vec<PathBuf> {
    let keep = under.to_path_buf();
    let mut builder = crate::index::walk_builder(from, is_git_repo);
    builder
        .hidden(false)
        .git_ignore(true)
        .git_exclude(true)
        .filter_entry(move |e| {
            e.file_name() != ".git" && (e.path().starts_with(&keep) || keep.starts_with(e.path()))
        });
    builder
        .build()
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_some_and(|t| t.is_dir()) && e.path().starts_with(under))
        .map(|e| e.into_path())
        .collect()
}

/// Every folder at or under `dir`, no filters (for `refs/`). Empty when `dir` is missing.
fn all_dirs(dir: &Path) -> Vec<PathBuf> {
    ignore::WalkBuilder::new(dir)
        .standard_filters(false)
        .build()
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_some_and(|t| t.is_dir()))
        .map(|e| e.into_path())
        .collect()
}

/// Whether watching `dirs` folders leaves the system enough inotify watches: at most a quarter of
/// the per-user `limit`, which every program shares. An unknown limit never refuses.
pub fn within_budget(dirs: usize, limit: Option<usize>) -> bool {
    limit.is_none_or(|limit| dirs.saturating_mul(4) <= limit)
}

/// The live watcher: FSEvents on macOS, inotify on Linux, ReadDirectoryChangesW on Windows. notify
/// runs its own thread and calls back; the callback forwards each batch over a channel the
/// controller drains every tick.
pub struct NotifyWatch;

/// Whether an event can have changed what the viewer shows. inotify also reports OPEN and
/// CLOSE_NOWRITE, so the viewer's own reads (`git status`, renderers) would otherwise come back as
/// events and each refresh would trigger the next. A finished write (`Close(Write)`) is a real change.
fn changes_content(kind: &notify::EventKind) -> bool {
    use notify::EventKind::Access;
    use notify::event::{AccessKind, AccessMode};
    !matches!(kind, Access(k) if !matches!(k, AccessKind::Close(AccessMode::Write)))
}

/// The batch to forward for one notify callback, if any.
fn forward(res: notify::Result<notify::Event>) -> Option<WatchEvent> {
    match res {
        Ok(ev) if ev.need_rescan() => Some(WatchEvent::Rescan),
        Ok(ev) if !changes_content(&ev.kind) => None,
        Ok(ev) => Some(WatchEvent::Paths(ev.paths)),
        // A backend error mid-session (say, a folder created past the watch limit) is not fatal:
        // keep forwarding what still arrives.
        Err(_) => None,
    }
}

/// Symlinks are not followed, as the tree does not follow them: on Linux a followed link to
/// `$HOME` or a shared store would walk and watch that whole tree, burning the user-wide inotify
/// watch limit for nothing the viewer shows.
fn config() -> notify::Config {
    notify::Config::default().with_follow_symlinks(false)
}

/// macOS and Windows watch a whole tree with one recursive watch, so they watch the root and
/// each git dir outside it recursively.
#[cfg(not(target_os = "linux"))]
impl WatchService for NotifyWatch {
    fn watch(&self, root: &Path, git_dirs: &[PathBuf], _is_git_repo: bool) -> Option<WatchHandle> {
        use notify::Watcher;
        let (tx, rx) = mpsc::channel();
        let handler = move |res| {
            if let Some(event) = forward(res) {
                let _ = tx.send(event);
            }
        };
        let mut watcher = notify::RecommendedWatcher::new(handler, config()).ok()?;
        watcher.watch(root, notify::RecursiveMode::Recursive).ok()?;
        for dir in git_dirs.iter().filter(|d| !d.starts_with(root)) {
            watcher.watch(dir, notify::RecursiveMode::Recursive).ok()?;
        }
        Some(WatchHandle {
            rx,
            guard: Box::new(watcher),
        })
    }
}

/// inotify needs one watch per folder, from a per-user limit every program shares, so Linux
/// watches only [`watch_dirs`], and refuses when that would take more than a quarter of the limit.
/// A folder created later is watched by a helper thread: the callback runs on notify's event-loop
/// thread, which `Watcher::watch` waits on, so calling it from the callback would deadlock.
#[cfg(target_os = "linux")]
impl WatchService for NotifyWatch {
    fn watch(&self, root: &Path, git_dirs: &[PathBuf], is_git_repo: bool) -> Option<WatchHandle> {
        use notify::Watcher;
        use notify::event::{CreateKind, EventKind, ModifyKind, RenameMode};
        use std::sync::{Arc, Mutex};
        let dirs = watch_dirs(root, git_dirs, is_git_repo);
        if !within_budget(dirs.len(), inotify_watch_limit()) {
            return None;
        }
        let (tx, rx) = mpsc::channel();
        let (new_dir_tx, new_dir_rx) = mpsc::channel::<PathBuf>();
        let helper_tx = tx.clone();
        let handler = move |res: notify::Result<notify::Event>| {
            if let Ok(ev) = &res
                && matches!(
                    ev.kind,
                    EventKind::Create(CreateKind::Folder)
                        | EventKind::Modify(ModifyKind::Name(RenameMode::To))
                )
            {
                for path in &ev.paths {
                    let _ = new_dir_tx.send(path.clone());
                }
            }
            if let Some(event) = forward(res) {
                let _ = tx.send(event);
            }
        };
        let mut watcher = notify::RecommendedWatcher::new(handler, config()).ok()?;
        for dir in &dirs {
            if let Err(err) = watcher.watch(dir, notify::RecursiveMode::NonRecursive)
                && !vanished(&err, dir, root)
            {
                return None;
            }
        }
        // The guard holds the only strong reference. Dropping the handle drops the watcher, which
        // drops the callback and its `new_dir_tx`, which ends the helper's loop: no cycle.
        let watcher = Arc::new(Mutex::new(watcher));
        let weak = Arc::downgrade(&watcher);
        let (root, git_dirs) = (root.to_path_buf(), git_dirs.to_vec());
        std::thread::spawn(move || {
            for dir in new_dir_rx {
                let added = new_dir_watch_dirs(&dir, &root, &git_dirs, is_git_repo);
                if added.is_empty() {
                    continue;
                }
                let Some(watcher) = weak.upgrade() else {
                    return;
                };
                if let Ok(mut watcher) = watcher.lock() {
                    for dir in &added {
                        let _ = watcher.watch(dir, notify::RecursiveMode::NonRecursive);
                    }
                }
                drop(watcher);
                // Files can land in the new folder before its watch exists: rescan to catch them.
                let _ = helper_tx.send(WatchEvent::Rescan);
            }
        });
        Some(WatchHandle {
            rx,
            guard: Box::new(watcher),
        })
    }
}

/// Whether a failed watch on `dir` only means it was deleted since the walk: skip it, rather than
/// turn live refresh off for the session. Any other error (the watch limit, say), or a missing
/// root, still refuses.
#[cfg(any(target_os = "linux", test))]
fn vanished(err: &notify::Error, dir: &Path, root: &Path) -> bool {
    let not_found = match &err.kind {
        notify::ErrorKind::PathNotFound => true,
        // notify stats a folder right after watching it, so one deleted in between is an io error.
        notify::ErrorKind::Io(e) => e.kind() == std::io::ErrorKind::NotFound,
        _ => false,
    };
    not_found && dir != root
}

/// The per-user inotify watch limit, or `None` when it cannot be read.
#[cfg(target_os = "linux")]
fn inotify_watch_limit() -> Option<usize> {
    std::fs::read_to_string("/proc/sys/fs/inotify/max_user_watches")
        .ok()?
        .trim()
        .parse()
        .ok()
}

#[cfg(test)]
mod tests {
    use super::{changes_content, vanished};
    use std::path::Path;

    #[test]
    fn only_a_vanished_folder_other_than_the_root_is_skipped() {
        let (root, dir) = (Path::new("/r"), Path::new("/r/gone"));
        let not_found = notify::Error::path_not_found();
        assert!(vanished(&not_found, dir, root));
        assert!(!vanished(&not_found, root, root), "a missing root refuses");
        let io_not_found = notify::Error::io(std::io::ErrorKind::NotFound.into());
        assert!(vanished(&io_not_found, dir, root));
        assert!(
            !vanished(&io_not_found, root, root),
            "a missing root refuses"
        );
        let io_other = notify::Error::io(std::io::ErrorKind::PermissionDenied.into());
        assert!(!vanished(&io_other, dir, root), "another io error refuses");
        let limit = notify::Error::new(notify::ErrorKind::MaxFilesWatch);
        assert!(!vanished(&limit, dir, root), "the watch limit refuses");
    }
    use notify::EventKind::{Access, Create, Modify, Remove};
    use notify::event::{AccessKind, AccessMode, CreateKind, DataChange, ModifyKind, RemoveKind};

    #[test]
    fn reads_are_dropped_and_real_changes_kept() {
        for dropped in [
            Access(AccessKind::Open(AccessMode::Any)),
            Access(AccessKind::Read),
            Access(AccessKind::Close(AccessMode::Read)),
        ] {
            assert!(!changes_content(&dropped), "{dropped:?}");
        }
        for kept in [
            Access(AccessKind::Close(AccessMode::Write)),
            Create(CreateKind::Any),
            Modify(ModifyKind::Data(DataChange::Any)),
            Remove(RemoveKind::Any),
        ] {
            assert!(changes_content(&kept), "{kept:?}");
        }
    }
}
