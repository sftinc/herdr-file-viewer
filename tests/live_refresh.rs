//! Live refresh (spec Part 1b): a watcher event, debounced, starts a background status job; when
//! it lands the tree, markers and active preview update in place, keeping scroll, search and view
//! overrides. The watcher is a fake the test drives, and time is injected into `tick_watch`, so
//! nothing here sleeps to prove a negative: "no render" is an unchanged `render_seq`.

mod common;

use common::TempDir;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use herdr_file_viewer::controller::{
    Components, ContentProvider, Controller, EditorHandoff, EditorOutcome, GitService,
    RenderResult, RootProviders,
};
use herdr_file_viewer::git::{Baseline, Status};
use herdr_file_viewer::intent::Intent;
use herdr_file_viewer::view_policy::ViewMode;
use herdr_file_viewer::watch::{WatchEvent, WatchHandle, WatchService};
use ratatui::text::Text;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Armed by a test: the next call reports it started, then blocks until released.
type Gate = (mpsc::Sender<()>, mpsc::Receiver<()>);

fn arm(gate: &Arc<Mutex<Option<Gate>>>) -> (mpsc::Receiver<()>, mpsc::Sender<()>) {
    let (started_tx, started_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    *gate.lock().unwrap() = Some((started_tx, release_rx));
    (started_rx, release_tx)
}

/// Taken (not borrowed) so only the first call after arming blocks, and the lock is never held
/// while blocked.
fn pass(gate: &Arc<Mutex<Option<Gate>>>) {
    let armed = gate.lock().unwrap().take();
    if let Some((started, release)) = armed {
        started.send(()).unwrap();
        release.recv().unwrap();
    }
}

/// A watcher the test drives: records every `watch` call and keeps the latest event sender.
#[derive(Clone, Default)]
struct FakeWatch {
    calls: Arc<Mutex<Vec<Vec<PathBuf>>>>,
    tx: Arc<Mutex<Option<mpsc::Sender<WatchEvent>>>>,
    fail: bool,
}
impl WatchService for FakeWatch {
    fn watch(&self, paths: &[PathBuf]) -> Option<WatchHandle> {
        self.calls.lock().unwrap().push(paths.to_vec());
        if self.fail {
            return None;
        }
        let (tx, rx) = mpsc::channel();
        *self.tx.lock().unwrap() = Some(tx);
        Some(WatchHandle {
            rx,
            guard: Box::new(()),
        })
    }
}
impl FakeWatch {
    fn send(&self, event: WatchEvent) {
        self.tx
            .lock()
            .unwrap()
            .as_ref()
            .expect("watching")
            .send(event)
            .unwrap();
    }
}

/// Git whose status the test changes between refreshes; `changed_set` mirrors status.
#[derive(Clone, Default)]
struct SharedGit {
    status: Arc<Mutex<BTreeMap<PathBuf, Status>>>,
    status_calls: Arc<Mutex<usize>>,
    dir_diffs: Arc<Mutex<usize>>,
    gate: Arc<Mutex<Option<Gate>>>,
}
impl SharedGit {
    fn set(&self, rel: &str, s: Status) {
        self.status.lock().unwrap().insert(PathBuf::from(rel), s);
    }
}
impl GitService for SharedGit {
    fn status(&self) -> BTreeMap<PathBuf, Status> {
        *self.status_calls.lock().unwrap() += 1;
        pass(&self.gate);
        self.status.lock().unwrap().clone()
    }
    fn changed_set(&self, _: Baseline) -> BTreeMap<PathBuf, Status> {
        self.status.lock().unwrap().clone()
    }
    fn diff(&self, _: &Path, _: Baseline, _: bool) -> String {
        String::new()
    }
    fn diff_directory(&self, _: &Path, _: Baseline) -> String {
        *self.dir_diffs.lock().unwrap() += 1;
        String::new()
    }
}

/// Renders `<name> <mode> #<n>` (n counts renders) and 59 more lines, so a test can tell renders
/// apart, see the view mode, and scroll.
#[derive(Clone, Default)]
struct Body {
    count: Arc<Mutex<usize>>,
    gate: Arc<Mutex<Option<Gate>>>,
}
impl ContentProvider for Body {
    fn render(&self, path: &Path, mode: ViewMode, _raw: Option<&str>) -> RenderResult {
        pass(&self.gate);
        let n = {
            let mut c = self.count.lock().unwrap();
            *c += 1;
            *c
        };
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        let mut s = format!("{name} {mode:?} #{n}");
        for i in 1..60 {
            s.push_str(&format!("\nline{i}"));
        }
        RenderResult {
            content: Text::raw(s),
            notices: Vec::new(),
            source: None,
        }
    }
}

struct NoEditor;
impl EditorHandoff for NoEditor {
    fn open(&mut self, _: &Path) -> EditorOutcome {
        EditorOutcome::NoTakeover
    }
}

struct Rig {
    ctrl: Controller,
    watch: FakeWatch,
    git: SharedGit,
    body: Body,
    /// Canonical: the filter compares against real paths, as the OS watcher reports them.
    root: PathBuf,
}

fn rig_with(dir: &Path, is_git: bool, watch: FakeWatch) -> Rig {
    let git = SharedGit::default();
    let body = Body::default();
    let (g, b) = (git.clone(), body.clone());
    let components = Components {
        providers: Box::new(move |_resolved| RootProviders {
            git: Arc::new(g.clone()),
            content: Box::new(b.clone()),
        }),
        editor: Box::new(NoEditor),
        clipboard: Box::new(common::RecordingClipboard::default()),
        renderers: None,
    };
    let mut ctrl = Controller::new(
        common::resolved(dir.to_path_buf(), is_git),
        Baseline::Head,
        components,
    );
    ctrl.set_watcher(Box::new(watch.clone()));
    Rig {
        ctrl,
        watch,
        git,
        body,
        root: dir.canonicalize().unwrap(),
    }
}

fn rig(dir: &Path, is_git: bool) -> Rig {
    rig_with(dir, is_git, FakeWatch::default())
}

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn text(ctrl: &Controller) -> String {
    ctrl.content()
        .lines
        .iter()
        .map(|l| {
            l.spans
                .iter()
                .map(|s| s.content.as_ref())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn selected(ctrl: &Controller) -> String {
    let node = ctrl.tree().selected().expect("a selection");
    node.path
        .file_name()
        .unwrap()
        .to_string_lossy()
        .into_owned()
}

/// Poll until `done` (a bounded positive wait, never a negative).
fn await_until(ctrl: &mut Controller, mut done: impl FnMut(&Controller) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        ctrl.poll();
        if done(ctrl) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "timed out; content was {:?}",
            text(ctrl)
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn await_text(ctrl: &mut Controller, marker: &str) {
    await_until(ctrl, |c| text(c).contains(marker));
}

/// The marker the NEXT render will carry (`#<n>`), so a test can await one specific render.
fn next_render(rig: &Rig) -> String {
    format!("#{}", *rig.body.count.lock().unwrap() + 1)
}

/// Wait for the background status job to land and be applied.
fn settle(ctrl: &mut Controller) {
    await_until(ctrl, |c| !c.watch_refresh_in_flight());
}

/// Run the debounce to its end with injected time; true when a refresh started.
fn fire(rig: &mut Rig, event: WatchEvent) -> bool {
    rig.watch.send(event);
    let t0 = Instant::now();
    assert!(
        !rig.ctrl.tick_watch(t0),
        "an event alone does not refresh yet"
    );
    rig.ctrl.tick_watch(t0 + Duration::from_millis(300))
}

/// One changed path, debounced, status job landed.
fn change(rig: &mut Rig, rel: &str) {
    let path = rig.root.join(rel);
    assert!(
        fire(rig, WatchEvent::Paths(vec![path])),
        "a relevant change starts a refresh"
    );
    settle(&mut rig.ctrl);
}

#[test]
fn a_refresh_keeps_scroll_search_and_view_override_and_reflows() {
    let dir = TempDir::new();
    std::fs::write(dir.path().join("a.md"), "# a\n").unwrap();
    let mut r = rig(dir.path(), true);
    await_text(&mut r.ctrl, "a.md RenderedMarkdown");
    r.ctrl.set_content_viewport(40, 10);

    r.ctrl.handle(Intent::CycleView); // a per-file override: away from rendered markdown
    // Wait for the real render, not the `Rendering…` placeholder (which also lacks the mode).
    await_until(&mut r.ctrl, |c| {
        text(c).starts_with("a.md ") && !text(c).contains("RenderedMarkdown")
    });
    let mode_line = text(&r.ctrl)
        .lines()
        .next()
        .unwrap()
        .split(" #")
        .next()
        .unwrap()
        .to_string();

    r.ctrl.handle(Intent::OpenSearch);
    for c in "line30".chars() {
        r.ctrl.handle_prompt_key(key(KeyCode::Char(c)));
    }
    r.ctrl.handle_prompt_key(key(KeyCode::Enter));
    let before = r.ctrl.view_state();
    assert!(
        before.active.search.is_some(),
        "precondition: a committed search"
    );
    assert!(
        before.active.scroll > 0,
        "precondition: scrolled to the match"
    );
    let seq = r.ctrl.render_seq();
    let reflow = next_render(&r);

    r.git.set("a.md", Status::Modified); // would default to Diff, but the override wins
    change(&mut r, "a.md");
    assert_eq!(
        r.ctrl.render_seq(),
        seq + 1,
        "exactly one background reflow"
    );
    await_text(&mut r.ctrl, &reflow);

    let after = r.ctrl.view_state();
    assert_eq!(after.active.scroll, before.active.scroll, "scroll kept");
    assert!(after.active.search.is_some(), "committed search kept");
    assert!(
        text(&r.ctrl).starts_with(&mode_line),
        "view override kept: {mode_line}"
    );
}

#[test]
fn a_refresh_rederives_the_view_mode_from_the_new_changed_set() {
    let dir = TempDir::new();
    std::fs::write(dir.path().join("a.rs"), "fn a() {}\n").unwrap();
    let mut r = rig(dir.path(), true);
    await_text(&mut r.ctrl, "a.rs SyntaxContent");
    r.git.set("a.rs", Status::Modified);
    change(&mut r, "a.rs");
    await_text(&mut r.ctrl, "a.rs Diff");
}

#[test]
fn a_file_appearing_above_the_selection_keeps_it() {
    let dir = TempDir::new();
    std::fs::write(dir.path().join("b.rs"), "b\n").unwrap();
    std::fs::write(dir.path().join("c.rs"), "c\n").unwrap();
    let mut r = rig(dir.path(), true);
    r.ctrl.handle(Intent::NavDown);
    await_text(&mut r.ctrl, "c.rs");
    let reflow = next_render(&r);
    std::fs::write(dir.path().join("a.rs"), "a\n").unwrap();
    change(&mut r, "a.rs");
    assert_eq!(selected(&r.ctrl), "c.rs");
    await_text(&mut r.ctrl, &format!("c.rs SyntaxContent {reflow}")); // c.rs reflowed in place
}

#[test]
fn a_deleted_selection_moves_to_the_neighbor_and_renders_it() {
    let dir = TempDir::new();
    std::fs::write(dir.path().join("a.rs"), "a\n").unwrap();
    std::fs::write(dir.path().join("b.rs"), "b\n").unwrap();
    let mut r = rig(dir.path(), true);
    r.ctrl.handle(Intent::NavDown);
    await_text(&mut r.ctrl, "b.rs");
    std::fs::remove_file(dir.path().join("b.rs")).unwrap();
    change(&mut r, "b.rs");
    assert_eq!(selected(&r.ctrl), "a.rs");
    await_text(&mut r.ctrl, "a.rs SyntaxContent");
}

#[test]
fn deleting_the_last_file_clears_the_preview() {
    let dir = TempDir::new();
    std::fs::write(dir.path().join("a.rs"), "a\n").unwrap();
    let mut r = rig(dir.path(), true);
    await_text(&mut r.ctrl, "a.rs");
    std::fs::remove_file(dir.path().join("a.rs")).unwrap();
    change(&mut r, "a.rs");
    assert!(r.ctrl.tree().selected().is_none());
    assert!(
        !text(&r.ctrl).contains("a.rs"),
        "the empty state, not the deleted file"
    );
}

#[test]
fn a_pending_line_select_render_is_not_cancelled() {
    let dir = TempDir::new();
    std::fs::write(dir.path().join("a.md"), "# a\n").unwrap();
    let mut r = rig(dir.path(), true);
    await_text(&mut r.ctrl, "a.md RenderedMarkdown");
    r.ctrl.set_content_viewport(40, 10);

    let (started, release) = arm(&r.body.gate);
    r.ctrl.enter_line_select_at_top(); // markdown: switches to source and waits for that render
    started.recv().unwrap();
    let seq = r.ctrl.render_seq();

    r.git.set("a.md", Status::Modified);
    change(&mut r, "a.md");
    assert_eq!(
        r.ctrl.render_seq(),
        seq,
        "no reflow while the L render is outstanding"
    );

    release.send(()).unwrap();
    await_until(&mut r.ctrl, |c| c.line_select_active());
}

#[test]
fn a_pending_go_to_line_render_is_not_cancelled() {
    let dir = TempDir::new();
    std::fs::write(dir.path().join("a.md"), "# a\n").unwrap();
    let mut r = rig(dir.path(), true);
    await_text(&mut r.ctrl, "a.md RenderedMarkdown");
    r.ctrl.set_content_viewport(40, 10);

    let (started, release) = arm(&r.body.gate);
    r.ctrl.handle(Intent::OpenGoToLine);
    r.ctrl.handle_prompt_key(key(KeyCode::Char('3')));
    r.ctrl.handle_prompt_key(key(KeyCode::Char('0')));
    r.ctrl.handle_prompt_key(key(KeyCode::Enter)); // markdown: switches to source, then jumps
    started.recv().unwrap();
    let seq = r.ctrl.render_seq();

    change(&mut r, "a.md");
    assert_eq!(
        r.ctrl.render_seq(),
        seq,
        "no reflow while the go-to-line render is outstanding"
    );

    release.send(()).unwrap();
    await_until(&mut r.ctrl, |c| c.view_state().active.scroll > 0);
}

#[test]
fn a_pinned_preview_is_unchanged_while_the_active_side_refreshes() {
    let dir = TempDir::new();
    std::fs::write(dir.path().join("a.rs"), "a\n").unwrap();
    std::fs::write(dir.path().join("b.rs"), "b\n").unwrap();
    let mut r = rig(dir.path(), true);
    await_text(&mut r.ctrl, "a.rs");
    r.ctrl.set_content_viewport(40, 10);
    r.ctrl.handle(Intent::PinPreview);
    r.ctrl.handle(Intent::NavDown);
    await_text(&mut r.ctrl, "b.rs");
    let pinned = |c: &Controller| {
        let view = c.view_state();
        let p = view.pinned.as_ref().expect("a pinned preview");
        p.content.lines[0]
            .spans
            .iter()
            .map(|s| s.content.to_string())
            .collect::<String>()
    };
    let pinned_before = pinned(&r.ctrl);
    let seq = r.ctrl.render_seq();

    r.git.set("a.rs", Status::Modified);
    r.git.set("b.rs", Status::Modified);
    change(&mut r, "a.rs");
    assert_eq!(r.ctrl.render_seq(), seq + 1, "the active side reflows");
    await_text(&mut r.ctrl, "b.rs Diff");
    assert_eq!(pinned(&r.ctrl), pinned_before, "the pin is frozen");
}

#[test]
fn a_directory_diff_in_status_mode_stays_a_directory_diff() {
    let dir = TempDir::new();
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    std::fs::write(dir.path().join("src/a.rs"), "a\n").unwrap();
    let mut r = rig(dir.path(), true);
    r.git.set("src/a.rs", Status::Modified);
    r.ctrl.handle(Intent::Refresh); // pick the status up
    r.ctrl.handle(Intent::ToggleStatusMode); // the tree is now `src` / `src/a.rs`; `src` selected
    assert_eq!(selected(&r.ctrl), "src");
    let dirs = Arc::clone(&r.git.dir_diffs);
    await_until(&mut r.ctrl, |_| *dirs.lock().unwrap() == 1);

    change(&mut r, "src/a.rs");
    await_until(&mut r.ctrl, |_| *dirs.lock().unwrap() == 2);
}

#[test]
fn re_root_replaces_the_watcher() {
    let dir = TempDir::new();
    std::fs::write(dir.path().join("a.rs"), "a\n").unwrap();
    let other = TempDir::new();
    std::fs::write(other.path().join("z.rs"), "z\n").unwrap();
    let mut r = rig(dir.path(), false);
    r.ctrl.re_root(other.path());
    let calls = r.watch.calls.lock().unwrap().clone();
    assert_eq!(calls.len(), 2, "one watch per root");
    assert_eq!(calls[1][0], other.path().canonicalize().unwrap());
    assert!(r.ctrl.watching());
}

#[test]
fn re_root_drops_an_in_flight_watcher_status() {
    let dir = TempDir::new();
    std::fs::write(dir.path().join("a.rs"), "a\n").unwrap();
    let other = TempDir::new();
    let mut r = rig(dir.path(), true);
    let (started, release) = arm(&r.git.gate);
    let path = r.root.join("a.rs");
    assert!(fire(&mut r, WatchEvent::Paths(vec![path])));
    started.recv().unwrap();
    r.ctrl.re_root(other.path());
    assert!(
        !r.ctrl.watch_refresh_in_flight(),
        "the old root's status job is dropped"
    );
    release.send(()).unwrap();
}

#[test]
fn focus_gain_during_a_watcher_status_still_reflows() {
    let dir = TempDir::new();
    std::fs::write(dir.path().join("a.rs"), "a\n").unwrap();
    let mut r = rig(dir.path(), true);
    await_text(&mut r.ctrl, "a.rs");
    let (started, release) = arm(&r.git.gate);
    let path = r.root.join("a.rs");
    assert!(fire(&mut r, WatchEvent::Paths(vec![path])));
    started.recv().unwrap();
    let seq = r.ctrl.render_seq();

    r.ctrl.handle_focus_gained(); // fresher status; the watcher job is dropped
    assert!(!r.ctrl.watch_refresh_in_flight());
    r.ctrl.poll();
    assert_eq!(
        r.ctrl.render_seq(),
        seq + 1,
        "the reflow the watcher owed is still sent"
    );
    release.send(()).unwrap();
}

#[test]
fn a_trigger_during_a_status_job_runs_exactly_one_more() {
    let dir = TempDir::new();
    let mut r = rig(dir.path(), true);
    let calls = Arc::clone(&r.git.status_calls);
    let base = *calls.lock().unwrap();
    let (started, release) = arm(&r.git.gate);
    assert!(fire(&mut r, WatchEvent::Rescan));
    started.recv().unwrap(); // job 1 is running and held
    assert!(fire(&mut r, WatchEvent::Rescan)); // sets the pending bit
    assert!(fire(&mut r, WatchEvent::Rescan)); // still one bit, not a queue
    release.send(()).unwrap();
    await_until(&mut r.ctrl, |c| {
        !c.watch_refresh_in_flight() && *calls.lock().unwrap() == base + 2
    });
    assert_eq!(
        *calls.lock().unwrap(),
        base + 2,
        "one job, then exactly one more"
    );
}

#[test]
fn a_rescan_triggers_a_refresh() {
    let dir = TempDir::new();
    let mut r = rig(dir.path(), true);
    assert!(fire(&mut r, WatchEvent::Rescan));
    settle(&mut r.ctrl);
}

#[test]
fn an_ignored_path_does_not_trigger_a_refresh() {
    let dir = TempDir::new();
    std::fs::write(dir.path().join(".gitignore"), "target/\n").unwrap();
    let mut r = rig(dir.path(), true);
    let path = r.root.join("target/debug/app");
    assert!(!fire(&mut r, WatchEvent::Paths(vec![path])));
    assert!(!r.ctrl.watch_refresh_in_flight());
}

#[test]
fn an_edited_gitignore_applies_to_later_events() {
    let dir = TempDir::new();
    let mut r = rig(dir.path(), true);
    std::fs::write(dir.path().join(".gitignore"), "*.log\n").unwrap();
    change(&mut r, ".gitignore");
    let path = r.root.join("build.log");
    assert!(
        !fire(&mut r, WatchEvent::Paths(vec![path])),
        "the new rule drops it"
    );
}

#[test]
fn outside_a_repo_a_change_redraws_without_a_status_job() {
    let dir = TempDir::new();
    std::fs::write(dir.path().join("a.rs"), "a\n").unwrap();
    let mut r = rig(dir.path(), false);
    std::fs::write(dir.path().join("b.rs"), "b\n").unwrap();
    let path = r.root.join("b.rs");
    assert!(
        fire(&mut r, WatchEvent::Paths(vec![path])),
        "redraw: the tree listing changed"
    );
    assert!(!r.ctrl.watch_refresh_in_flight(), "no git, no status job");
}

#[test]
fn a_watcher_that_fails_to_start_leaves_focus_refresh_working() {
    let dir = TempDir::new();
    std::fs::write(dir.path().join("a.rs"), "a\n").unwrap();
    let mut r = rig_with(
        dir.path(),
        true,
        FakeWatch {
            fail: true,
            ..FakeWatch::default()
        },
    );
    assert!(!r.ctrl.watching());
    assert!(!r.ctrl.tick_watch(Instant::now()));
    r.git.set("a.rs", Status::Modified);
    r.ctrl.handle_focus_gained();
    let nodes = r.ctrl.tree().visible_nodes();
    assert_eq!(
        nodes[0].status,
        Some(Status::Modified),
        "focus refresh still reads git"
    );
}

#[test]
fn a_watcher_that_stops_falls_back_silently() {
    let dir = TempDir::new();
    let mut r = rig(dir.path(), true);
    *r.watch.tx.lock().unwrap() = None; // drops the sender: the channel disconnects
    assert!(!r.ctrl.tick_watch(Instant::now()));
    assert!(!r.ctrl.watching());
}
