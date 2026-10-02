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
/// Sync`, because setup runs off the UI thread: inotify walks the whole root to add its watches.
pub trait WatchService: Send + Sync {
    /// Watch every path in `paths` recursively. `None` when the watcher cannot start (for example
    /// the Linux inotify watch limit); the viewer then refreshes on focus and `r` only.
    fn watch(&self, paths: &[PathBuf]) -> Option<WatchHandle>;
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

impl WatchService for NotifyWatch {
    fn watch(&self, paths: &[PathBuf]) -> Option<WatchHandle> {
        use notify::Watcher;
        let (tx, rx) = mpsc::channel();
        let handler = move |res: notify::Result<notify::Event>| {
            let event = match res {
                Ok(ev) if ev.need_rescan() => WatchEvent::Rescan,
                Ok(ev) if !changes_content(&ev.kind) => return,
                Ok(ev) => WatchEvent::Paths(ev.paths),
                // A backend error mid-session (say, a folder created past the watch limit) is not
                // fatal: keep forwarding what still arrives.
                Err(_) => return,
            };
            let _ = tx.send(event);
        };
        // Symlinks are not followed, as the tree does not follow them: on Linux a followed link to
        // `$HOME` or a shared store would walk and watch that whole tree, burning the user-wide
        // inotify watch limit for nothing the viewer shows.
        let config = notify::Config::default().with_follow_symlinks(false);
        let mut watcher = notify::RecommendedWatcher::new(handler, config).ok()?;
        for path in paths {
            watcher.watch(path, notify::RecursiveMode::Recursive).ok()?;
        }
        Some(WatchHandle {
            rx,
            guard: Box::new(watcher),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::changes_content;
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
