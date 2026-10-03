//! Live refresh (spec Part 1b): a watcher event, debounced, starts a background status job; when
//! it lands the tree, markers and active preview update in place, keeping scroll, search and view
//! overrides. The watcher is a fake the test drives, and time is injected into `tick_watch`, so
//! nothing here sleeps to prove a negative: "no render" is an unchanged `render_seq`.

mod common;

use common::TempDir;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use herdr_file_viewer::controller::{
    Components, ContentProvider, Controller, EditorHandoff, EditorOutcome, GitService,
    RenderResult, RootProviders,
};
use herdr_file_viewer::git::{Baseline, Status};
use herdr_file_viewer::intent::Intent;
use herdr_file_viewer::presenter::PaneGeometry;
use herdr_file_viewer::view_policy::ViewMode;
use herdr_file_viewer::watch::{WatchEvent, WatchHandle, WatchService};
use ratatui::layout::Rect;
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

/// One `watch` call: its root and git dirs.
type WatchCall = (PathBuf, Vec<PathBuf>);

/// A watcher the test drives: records every `watch` call and keeps the latest event sender. The
/// gate holds a `watch` call open, as a slow inotify walk would.
#[derive(Clone, Default)]
struct FakeWatch {
    calls: Arc<Mutex<Vec<WatchCall>>>,
    tx: Arc<Mutex<Option<mpsc::Sender<WatchEvent>>>>,
    fail: bool,
    gate: Arc<Mutex<Option<Gate>>>,
}
impl WatchService for FakeWatch {
    fn watch(&self, root: &Path, git_dirs: &[PathBuf], _is_git_repo: bool) -> Option<WatchHandle> {
        pass(&self.gate);
        self.calls
            .lock()
            .unwrap()
            .push((root.to_path_buf(), git_dirs.to_vec()));
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
    // Watch setup runs off the UI thread; wait for it to land (installed, or failed to start).
    await_until(&mut ctrl, |c| !c.watch_starting());
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
    assert!(
        r.ctrl
            .tree()
            .visible_nodes()
            .iter()
            .any(|n| n.path.file_name().is_some_and(|f| f == "a.rs")),
        "the new file is listed"
    );
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
    await_until(&mut r.ctrl, |c| c.watching()); // setup is off-thread
    let calls = r.watch.calls.lock().unwrap().clone();
    assert_eq!(calls.len(), 2, "one watch per root");
    assert_eq!(calls[1].0, other.path().canonicalize().unwrap());
    assert!(r.ctrl.watching());
}

#[test]
fn the_watcher_gets_the_canonical_root_and_every_git_dir_even_inside_the_root() {
    let dir = TempDir::new();
    common::init_repo_with_commit(dir.path());
    let r = rig(dir.path(), true);
    let calls = r.watch.calls.lock().unwrap().clone();
    assert_eq!(calls, vec![(r.root.clone(), vec![r.root.join(".git")])]);
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
    assert!(
        r.ctrl
            .tree()
            .visible_nodes()
            .iter()
            .any(|n| n.path.file_name().is_some_and(|f| f == "b.rs")),
        "the new file is listed"
    );
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

fn mouse(kind: MouseEventKind, col: u16, row: u16) -> MouseEvent {
    MouseEvent {
        kind,
        column: col,
        row,
        modifiers: KeyModifiers::NONE,
    }
}

/// Content interior starts at screen row 1, column 41 (as `tests/lineselect.rs`).
fn content_geometry() -> PaneGeometry {
    PaneGeometry {
        content_inner: Some(Rect {
            x: 41,
            y: 1,
            width: 58,
            height: 20,
        }),
        divider_x: Some(40),
        ..PaneGeometry::default()
    }
}

#[test]
fn a_reflow_waits_while_line_select_is_open() {
    let dir = TempDir::new();
    std::fs::write(dir.path().join("a.rs"), "a\n").unwrap();
    let mut r = rig(dir.path(), true);
    await_text(&mut r.ctrl, "a.rs");
    r.ctrl.set_content_viewport(40, 10);
    r.ctrl.enter_line_select_at_top();
    assert!(r.ctrl.line_select_active());
    let seq = r.ctrl.render_seq();

    r.git.set("a.rs", Status::Modified);
    change(&mut r, "a.rs");
    assert_eq!(
        r.ctrl.render_seq(),
        seq,
        "held: the text under the selection must not move"
    );

    r.ctrl.exit_line_select();
    r.ctrl.poll();
    assert_eq!(r.ctrl.render_seq(), seq + 1, "sent once line select closes");
}

#[test]
fn a_reflow_waits_during_a_content_drag() {
    let dir = TempDir::new();
    std::fs::write(dir.path().join("a.rs"), "a\n").unwrap();
    let mut r = rig(dir.path(), true);
    await_text(&mut r.ctrl, "a.rs");
    r.ctrl.set_content_viewport(80, 20);
    r.ctrl.set_pane_geometry(content_geometry());
    r.ctrl
        .handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 41, 1));
    r.ctrl
        .handle_mouse(mouse(MouseEventKind::Drag(MouseButton::Left), 44, 2));
    let seq = r.ctrl.render_seq();

    r.git.set("a.rs", Status::Modified);
    change(&mut r, "a.rs");
    assert_eq!(
        r.ctrl.render_seq(),
        seq,
        "held while the drag selects content"
    );

    r.ctrl
        .handle_mouse(mouse(MouseEventKind::Up(MouseButton::Left), 44, 2));
    r.ctrl.poll();
    assert_eq!(r.ctrl.render_seq(), seq + 1, "sent once the drag ends");
}

#[test]
fn a_deleted_selection_closes_line_select() {
    let dir = TempDir::new();
    std::fs::write(dir.path().join("a.rs"), "a\n").unwrap();
    std::fs::write(dir.path().join("b.rs"), "b\n").unwrap();
    let mut r = rig(dir.path(), true);
    r.ctrl.handle(Intent::NavDown);
    await_text(&mut r.ctrl, "b.rs");
    r.ctrl.set_content_viewport(40, 10);
    r.ctrl.enter_line_select_at_top();
    assert!(r.ctrl.line_select_active());

    std::fs::remove_file(dir.path().join("b.rs")).unwrap();
    change(&mut r, "b.rs");
    assert!(!r.ctrl.line_select_active(), "its file is gone");
    await_text(&mut r.ctrl, "a.rs");
}

#[test]
fn cancelling_the_annotation_editor_after_a_reflow_does_not_restore_old_lines() {
    let dir = TempDir::new();
    std::fs::write(dir.path().join("a.rs"), "a\n").unwrap();
    let mut r = rig(dir.path(), true);
    await_text(&mut r.ctrl, "a.rs");
    r.ctrl.set_content_viewport(40, 10);
    r.ctrl.enter_line_select_at_top();
    r.ctrl.handle_line_select_key(key(KeyCode::Char('a'))); // add-annotation editor, with snapshot
    assert!(r.ctrl.annotation_editor().is_some());

    r.git.set("a.rs", Status::Modified);
    change(&mut r, "a.rs"); // line select is not open (the editor is), so the reflow goes out
    await_text(&mut r.ctrl, "a.rs Diff");

    r.ctrl.handle_annotation_editor_key(key(KeyCode::Esc));
    assert!(
        !r.ctrl.line_select_active(),
        "old line numbers are not restored onto new content"
    );
}

#[test]
fn a_full_render_settles_an_owed_reflow() {
    let dir = TempDir::new();
    std::fs::write(dir.path().join("a.rs"), "a\n").unwrap();
    let mut r = rig(dir.path(), true);
    await_text(&mut r.ctrl, "a.rs");

    let (started, release) = arm(&r.git.gate);
    let path = r.root.join("a.rs");
    assert!(fire(&mut r, WatchEvent::Paths(vec![path])));
    started.recv().unwrap();
    let full = next_render(&r);
    // A sync refresh drops the in-flight job, owes a reflow, then dispatches a full render.
    r.ctrl.handle(Intent::Refresh);
    let seq = r.ctrl.render_seq(); // the full render is dispatched
    await_text(&mut r.ctrl, &full);
    r.ctrl.poll();
    release.send(()).unwrap(); // before the assert, so a failure leaves no blocked thread
    assert_eq!(
        r.ctrl.render_seq(),
        seq,
        "no stale reflow once the full render lands"
    );
}

#[test]
fn cancelling_the_editor_while_a_reflow_is_in_flight_does_not_restore_old_lines() {
    let dir = TempDir::new();
    std::fs::write(dir.path().join("a.rs"), "a\n").unwrap();
    let mut r = rig(dir.path(), true);
    await_text(&mut r.ctrl, "a.rs");
    r.ctrl.set_content_viewport(40, 10);
    r.ctrl.enter_line_select_at_top();
    r.ctrl.handle_line_select_key(key(KeyCode::Char('a')));
    assert!(r.ctrl.annotation_editor().is_some());

    let (started, release) = arm(&r.body.gate);
    r.git.set("a.rs", Status::Modified);
    change(&mut r, "a.rs"); // the reflow is sent and held in the renderer
    started.recv().unwrap();

    r.ctrl.handle_annotation_editor_key(key(KeyCode::Esc));
    let restored = r.ctrl.line_select_active();
    release.send(()).unwrap();
    assert!(!restored, "the reflow in flight dooms the old line numbers");
}

/// Scroll the preview down with a committed search, so a test can see whether a later render
/// kept or reset them.
fn scroll_with_search(ctrl: &mut Controller) {
    ctrl.set_content_viewport(40, 10);
    ctrl.handle(Intent::OpenSearch);
    for c in "line30".chars() {
        ctrl.handle_prompt_key(key(KeyCode::Char(c)));
    }
    ctrl.handle_prompt_key(key(KeyCode::Enter));
    let state = ctrl.view_state();
    assert!(
        state.active.search.is_some(),
        "precondition: a committed search"
    );
    assert!(
        state.active.scroll > 0,
        "precondition: scrolled to the match"
    );
}

/// Review I1: the selection is deleted while the watcher's status job is in flight, and the pane
/// regains focus. The focus refresh drops the job and owes its reflow, but the selection has
/// already moved to the neighbour, so the owed reflow must become a fresh render of the neighbour,
/// not a reflow that carries the deleted file's scroll and search onto it.
#[test]
fn an_owed_reflow_after_the_selection_was_deleted_renders_the_neighbour_fresh() {
    let dir = TempDir::new();
    std::fs::write(dir.path().join("a.rs"), "a\n").unwrap();
    std::fs::write(dir.path().join("b.rs"), "b\n").unwrap();
    let mut r = rig(dir.path(), true);
    r.ctrl.handle(Intent::NavDown);
    await_text(&mut r.ctrl, "b.rs");
    scroll_with_search(&mut r.ctrl);

    std::fs::remove_file(dir.path().join("b.rs")).unwrap();
    let (started, release) = arm(&r.git.gate);
    let path = r.root.join("b.rs");
    assert!(fire(&mut r, WatchEvent::Paths(vec![path])));
    started.recv().unwrap(); // the watcher's status job is held
    assert_eq!(
        selected(&r.ctrl),
        "a.rs",
        "a draw re-anchored to the neighbour"
    );

    let next = next_render(&r);
    r.ctrl.handle_focus_gained();
    release.send(()).unwrap();
    await_text(&mut r.ctrl, &format!("a.rs SyntaxContent {next}"));
    let state = r.ctrl.view_state();
    assert_eq!(state.active.scroll, 0, "the neighbour starts at the top");
    assert!(
        state.active.search.is_none(),
        "the deleted file's search is not carried over"
    );
}

/// Review I1, second route: a reflow is held by line select, the selection is deleted, and line
/// select closes before the next watcher status lands. The held reflow must not reflow the
/// neighbour with the deleted file's scroll and search.
#[test]
fn a_held_reflow_after_the_selection_was_deleted_renders_the_neighbour_fresh() {
    let dir = TempDir::new();
    std::fs::write(dir.path().join("a.rs"), "a\n").unwrap();
    std::fs::write(dir.path().join("b.rs"), "b\n").unwrap();
    let mut r = rig(dir.path(), true);
    r.ctrl.handle(Intent::NavDown);
    await_text(&mut r.ctrl, "b.rs");
    scroll_with_search(&mut r.ctrl);
    r.ctrl.enter_line_select_at_top();
    assert!(r.ctrl.line_select_active());
    let seq = r.ctrl.render_seq();
    change(&mut r, "b.rs");
    assert_eq!(r.ctrl.render_seq(), seq, "precondition: the reflow is held");

    std::fs::remove_file(dir.path().join("b.rs")).unwrap();
    assert_eq!(
        selected(&r.ctrl),
        "a.rs",
        "a draw re-anchored to the neighbour"
    );
    r.ctrl.exit_line_select();
    let next = next_render(&r);
    await_text(&mut r.ctrl, &format!("a.rs SyntaxContent {next}"));
    let state = r.ctrl.view_state();
    assert_eq!(state.active.scroll, 0, "the neighbour starts at the top");
    assert!(
        state.active.search.is_none(),
        "the deleted file's search is not carried over"
    );
}

/// Review M4: with no watcher event at all (as with `watch = false`), the selected file is deleted
/// and the pane regains focus. Draws already re-anchored the selection to the neighbour, so the
/// focus refresh must compare against the rendered file, not the pre-refresh selection, and render
/// the neighbour instead of leaving the deleted file's body under its highlight.
#[test]
fn focus_gain_after_the_selection_was_deleted_renders_the_neighbour() {
    let dir = TempDir::new();
    std::fs::write(dir.path().join("a.rs"), "a\n").unwrap();
    std::fs::write(dir.path().join("b.rs"), "b\n").unwrap();
    let mut r = rig(dir.path(), true);
    r.ctrl.handle(Intent::NavDown);
    await_text(&mut r.ctrl, "b.rs");

    std::fs::remove_file(dir.path().join("b.rs")).unwrap();
    assert_eq!(
        selected(&r.ctrl),
        "a.rs",
        "a draw re-anchored to the neighbour"
    );
    let seq = r.ctrl.render_seq();
    let next = next_render(&r);
    r.ctrl.handle_focus_gained();
    assert_eq!(r.ctrl.render_seq(), seq + 1, "the neighbour is rendered");
    await_text(&mut r.ctrl, &format!("a.rs SyntaxContent {next}"));
}

/// Review I3 (AC-17): watcher setup (an inotify walk of the whole root on Linux) runs off the UI
/// thread, so a re-root returns while the new root's `watch` call is still held open.
#[test]
fn re_root_does_not_wait_for_the_watcher_to_start() {
    let dir = TempDir::new();
    let other = TempDir::new();
    let mut r = rig(dir.path(), false);
    let (started, release) = arm(&r.watch.gate);
    r.ctrl.re_root(other.path()); // returns although the watch call below is held
    started.recv().unwrap();
    let (starting, watching) = (r.ctrl.watch_starting(), r.ctrl.watching());
    release.send(()).unwrap(); // before the asserts, so a failure leaves no blocked thread
    assert!(starting, "setup is still running");
    assert!(!watching, "not watching until setup lands");
    await_until(&mut r.ctrl, |c| c.watching());
}
