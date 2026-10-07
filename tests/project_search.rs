mod common;

use common::TempDir;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use herdr_file_viewer::controller::{
    Components, ContentProvider, Controller, EditorHandoff, EditorOutcome, GitService,
    RenderResult, RootProviders,
};
use herdr_file_viewer::git::{Baseline, Status};
use herdr_file_viewer::intent::Intent;
use herdr_file_viewer::repo_search::{
    SearchControl, SearchHit, SearchOutput, SearchRequest, Searcher,
};
use herdr_file_viewer::view_policy::ViewMode;
use ratatui::text::Text;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

#[derive(Default)]
struct StubGit;

impl GitService for StubGit {
    fn status(&self) -> BTreeMap<PathBuf, Status> {
        BTreeMap::new()
    }

    fn changed_set(&self, _baseline: Baseline) -> BTreeMap<PathBuf, Status> {
        BTreeMap::new()
    }

    fn diff(&self, _rel_path: &Path, _baseline: Baseline, _full_context: bool) -> String {
        String::new()
    }

    fn diff_directory(&self, _rel_dir: &Path, _baseline: Baseline) -> String {
        String::new()
    }
}

#[derive(Clone, Copy)]
struct FileContent;

impl ContentProvider for FileContent {
    fn render(&self, path: &Path, _mode: ViewMode, _raw_diff: Option<&str>) -> RenderResult {
        RenderResult {
            content: Text::raw(std::fs::read_to_string(path).unwrap()),
            notices: Vec::new(),
            source: None,
        }
    }
}

struct NoopEditor;

impl EditorHandoff for NoopEditor {
    fn open(&mut self, _file: &Path) -> EditorOutcome {
        EditorOutcome::NoTakeover
    }
}

fn controller(root: &Path) -> Controller {
    controller_with_git(root, false)
}

fn controller_with_git(root: &Path, is_git_repo: bool) -> Controller {
    let components = Components {
        providers: Box::new(|_resolved| RootProviders {
            git: Arc::new(StubGit),
            content: Box::new(FileContent),
        }),
        editor: Box::new(NoopEditor),
        clipboard: Box::new(common::RecordingClipboard::default()),
        renderers: None,
    };
    Controller::new(
        common::resolved(root.to_path_buf(), is_git_repo),
        Baseline::Head,
        components,
    )
}

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn type_query(controller: &mut Controller, query: &str) {
    for c in query.chars() {
        controller.handle_project_search_key(key(KeyCode::Char(c)));
    }
}

/// Poll until the current query's scan has FINISHED with at least one hit. Results stream, so a
/// non-empty hit list alone may be a partial; the status chip leaves "Searching…" only once the
/// final completion is applied.
fn await_hits(controller: &mut Controller) -> Vec<SearchHit> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        controller.poll();
        let done = status(controller).is_some_and(|s| !s.starts_with("Searching"));
        if done
            && let Some(hits) = controller.project_search_hits()
            && !hits.is_empty()
        {
            return hits.to_vec();
        }
        assert!(Instant::now() < deadline, "project search did not finish");
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn status(controller: &Controller) -> Option<String> {
    controller.view_state().finder.and_then(|f| f.status)
}

#[test]
fn search_scope_follows_the_current_i_state() {
    let tmp = TempDir::new();
    std::fs::write(tmp.path().join(".gitignore"), "ignored.txt\n").unwrap();
    std::fs::write(tmp.path().join("visible.txt"), "scope needle\n").unwrap();
    std::fs::write(tmp.path().join("ignored.txt"), "scope needle\n").unwrap();
    let mut controller = controller(tmp.path());

    controller.handle(Intent::OpenProjectSearch);
    type_query(&mut controller, "scope needle");
    let project_hits = await_hits(&mut controller);
    assert_eq!(
        project_hits
            .iter()
            .map(|hit| hit.path.as_str())
            .collect::<Vec<_>>(),
        vec!["visible.txt"]
    );

    controller.handle_project_search_key(key(KeyCode::Esc));
    controller.handle(Intent::ToggleIgnore);
    controller.handle(Intent::OpenProjectSearch);
    type_query(&mut controller, "scope needle");
    let all_hits = await_hits(&mut controller);
    assert_eq!(
        all_hits
            .iter()
            .map(|hit| hit.path.as_str())
            .collect::<Vec<_>>(),
        vec!["ignored.txt", "visible.txt"]
    );
}

#[test]
fn enter_opens_the_selected_result_in_source_view_at_its_line() {
    let tmp = TempDir::new();
    std::fs::write(
        tmp.path().join("target.md"),
        "line one\nline two\nline three\nunique needle\nline five\nline six\n",
    )
    .unwrap();
    let mut controller = controller(tmp.path());
    controller.set_content_viewport(40, 2);

    controller.handle(Intent::OpenProjectSearch);
    type_query(&mut controller, "unique needle");
    let hits = await_hits(&mut controller);
    assert_eq!((hits[0].path.as_str(), hits[0].line), ("target.md", 4));

    controller.handle_project_search_key(key(KeyCode::Enter));
    let deadline = Instant::now() + Duration::from_secs(5);
    while controller.content_scroll() != 3 {
        controller.poll();
        assert!(Instant::now() < deadline, "source-line jump did not apply");
        std::thread::sleep(Duration::from_millis(5));
    }

    assert!(!controller.project_search_open());
    assert_eq!(
        controller.selected_view_mode(),
        Some(ViewMode::SyntaxContent)
    );
    let selected = controller
        .tree()
        .selected()
        .expect("search result selected");
    assert_eq!(
        selected.path.file_name().and_then(|name| name.to_str()),
        Some("target.md")
    );
}

// ── Worker ordering, cancellation and streaming (gated searchers) ─────────────────────────────
//
// These inject a searcher through `Controller::set_project_searcher` so the test decides when each
// scan starts and ends, instead of hoping a real walk happens to race. Every wait is a bounded
// `recv_timeout` on a signal the searcher sends, never a sleep.

const SIGNAL_TIMEOUT: Duration = Duration::from_secs(5);

fn hit(path: &str, line: usize) -> SearchHit {
    SearchHit {
        path: path.into(),
        line,
        column: 1,
        excerpt: "needle".into(),
    }
}

/// A searcher that reports each scan's query on `started`, then blocks until the test sends on
/// `release`. It ignores cancellation and returns one hit named after its query, so a superseded
/// scan still produces a (stale) completion the controller must discard.
fn gated_searcher() -> (Searcher, mpsc::Receiver<String>, mpsc::Sender<()>) {
    let (started_tx, started_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let release_rx = Mutex::new(release_rx);
    let searcher: Searcher = Arc::new(move |req: &SearchRequest, _: &SearchControl| {
        started_tx.send(req.query.clone()).unwrap();
        release_rx
            .lock()
            .unwrap()
            .recv_timeout(Duration::from_secs(30))
            .expect("the test releases every scan it starts");
        Some(SearchOutput {
            hits: vec![hit(&format!("{}.txt", req.query), 1)],
            limited: false,
        })
    });
    (searcher, started_rx, release_tx)
}

#[test]
fn a_superseded_scan_result_is_never_applied() {
    let tmp = TempDir::new();
    let mut controller = controller(tmp.path());
    let (searcher, started, release) = gated_searcher();
    controller.set_project_searcher(searcher);

    controller.handle(Intent::OpenProjectSearch);
    type_query(&mut controller, "a");
    assert_eq!(started.recv_timeout(SIGNAL_TIMEOUT).unwrap(), "a");

    // Dispatch "ab" while "a" is still held open, then let "a" finish with its stale result.
    type_query(&mut controller, "b");
    release.send(()).unwrap();
    // The single worker only starts "ab" after it has sent "a"'s completion, so that completion
    // is already waiting in the channel: this poll must see it and drop it.
    assert_eq!(started.recv_timeout(SIGNAL_TIMEOUT).unwrap(), "ab");
    controller.poll();
    assert_eq!(
        controller.project_search_hits(),
        Some(&[][..]),
        "the superseded \"a\" scan's rows must not be shown for the query \"ab\""
    );
    assert_eq!(status(&controller).as_deref(), Some("Searching…"));

    release.send(()).unwrap();
    let hits = await_hits(&mut controller);
    assert_eq!(hits, vec![hit("ab.txt", 1)]);
}

#[test]
fn a_result_arriving_after_esc_does_not_reopen_or_fill_the_search() {
    let tmp = TempDir::new();
    let mut controller = controller(tmp.path());
    let (searcher, started, release) = gated_searcher();
    controller.set_project_searcher(searcher);

    controller.handle(Intent::OpenProjectSearch);
    type_query(&mut controller, "a");
    assert_eq!(started.recv_timeout(SIGNAL_TIMEOUT).unwrap(), "a");
    controller.handle_project_search_key(key(KeyCode::Esc));
    controller.handle(Intent::OpenProjectSearch);
    release.send(()).unwrap();

    // Prove the late completion has been sent: the next scan only starts after it.
    type_query(&mut controller, "z");
    assert_eq!(started.recv_timeout(SIGNAL_TIMEOUT).unwrap(), "z");
    controller.poll();
    assert_eq!(controller.project_search_hits(), Some(&[][..]));
    release.send(()).unwrap();
    assert_eq!(await_hits(&mut controller), vec![hit("z.txt", 1)]);
}

#[test]
fn editing_or_closing_cancels_the_running_scan() {
    let tmp = TempDir::new();
    let mut controller = controller(tmp.path());
    let (started_tx, started) = mpsc::channel();
    let (outcome_tx, outcome) = mpsc::channel();
    // Spins until the controller cancels it (bounded, so a missing cancel fails instead of hangs).
    let searcher: Searcher = Arc::new(move |req: &SearchRequest, ctl: &SearchControl| {
        started_tx.send(req.query.clone()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(30);
        while !(ctl.cancelled)() && Instant::now() < deadline {
            std::thread::yield_now();
        }
        outcome_tx
            .send((req.query.clone(), (ctl.cancelled)()))
            .unwrap();
        None
    });
    controller.set_project_searcher(searcher);

    controller.handle(Intent::OpenProjectSearch);
    type_query(&mut controller, "a");
    assert_eq!(started.recv_timeout(SIGNAL_TIMEOUT).unwrap(), "a");
    type_query(&mut controller, "b");
    assert_eq!(
        outcome.recv_timeout(SIGNAL_TIMEOUT).unwrap(),
        ("a".to_string(), true),
        "typing past a query cancels its scan"
    );

    assert_eq!(started.recv_timeout(SIGNAL_TIMEOUT).unwrap(), "ab");
    controller.handle_project_search_key(key(KeyCode::Esc));
    assert_eq!(
        outcome.recv_timeout(SIGNAL_TIMEOUT).unwrap(),
        ("ab".to_string(), true),
        "Esc cancels the running scan"
    );
}

#[test]
fn partial_results_show_while_the_scan_is_still_running() {
    let tmp = TempDir::new();
    let mut controller = controller(tmp.path());
    let (started_tx, started) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let release_rx = Mutex::new(release_rx);
    let searcher: Searcher = Arc::new(move |_: &SearchRequest, ctl: &SearchControl| {
        (ctl.partial)(&SearchOutput {
            hits: vec![hit("a.txt", 1)],
            limited: false,
        });
        started_tx.send(()).unwrap();
        release_rx
            .lock()
            .unwrap()
            .recv_timeout(Duration::from_secs(30))
            .expect("the test releases the scan");
        Some(SearchOutput {
            hits: vec![hit("a.txt", 1), hit("b.txt", 2)],
            limited: false,
        })
    });
    controller.set_project_searcher(searcher);

    controller.handle(Intent::OpenProjectSearch);
    type_query(&mut controller, "n");
    started.recv_timeout(SIGNAL_TIMEOUT).unwrap();
    // The partial was sent before `started`, so one poll applies it.
    controller.poll();
    assert_eq!(
        controller.project_search_hits(),
        Some(&[hit("a.txt", 1)][..])
    );
    assert_eq!(status(&controller).as_deref(), Some("Searching… 1 match"));

    release_tx.send(()).unwrap();
    let deadline = Instant::now() + SIGNAL_TIMEOUT;
    while controller
        .project_search_hits()
        .is_some_and(|h| h.len() < 2)
    {
        controller.poll();
        assert!(Instant::now() < deadline, "the final result did not arrive");
        std::thread::yield_now();
    }
    assert_eq!(status(&controller).as_deref(), Some("2 matches"));
}

/// The controller hands its resolved git-repo flag to every scan, so content search bounds the
/// ancestor `.gitignore` search exactly like the tree and Go-to-file (the scanner side is covered
/// in `tests/repo_search.rs`).
#[test]
fn scans_carry_the_roots_git_repo_flag() {
    for is_git_repo in [true, false] {
        let tmp = TempDir::new();
        let mut controller = controller_with_git(tmp.path(), is_git_repo);
        let (seen_tx, seen) = mpsc::channel();
        controller.set_project_searcher(Arc::new(move |req: &SearchRequest, _: &SearchControl| {
            seen_tx.send(req.is_git_repo).unwrap();
            Some(SearchOutput::default())
        }));

        controller.handle(Intent::OpenProjectSearch);
        type_query(&mut controller, "a");
        assert_eq!(seen.recv_timeout(SIGNAL_TIMEOUT).unwrap(), is_git_repo);
    }
}

/// Enter pressed before the current query has any result is held, not dropped: the first hit
/// opens when it arrives. Editing the query discards the held Enter.
#[test]
fn an_early_enter_opens_the_first_result_once_it_arrives() {
    let tmp = TempDir::new();
    std::fs::write(tmp.path().join("ab.txt"), "needle\n").unwrap();
    let mut controller = controller(tmp.path());
    let (searcher, started, release) = gated_searcher();
    controller.set_project_searcher(searcher);

    controller.handle(Intent::OpenProjectSearch);
    type_query(&mut controller, "a");
    assert_eq!(started.recv_timeout(SIGNAL_TIMEOUT).unwrap(), "a");
    controller.handle_project_search_key(key(KeyCode::Enter));
    assert!(
        controller.project_search_open(),
        "no result yet: Enter waits"
    );

    // Editing drops the held Enter: "ab"'s result is shown, not opened.
    type_query(&mut controller, "b");
    release.send(()).unwrap();
    assert_eq!(started.recv_timeout(SIGNAL_TIMEOUT).unwrap(), "ab");
    release.send(()).unwrap();
    assert_eq!(await_hits(&mut controller), vec![hit("ab.txt", 1)]);
    assert!(controller.project_search_open());

    // A fresh query with Enter pressed early opens its first hit on arrival.
    controller.handle_project_search_key(key(KeyCode::Backspace));
    assert_eq!(started.recv_timeout(SIGNAL_TIMEOUT).unwrap(), "a");
    type_query(&mut controller, "b");
    controller.handle_project_search_key(key(KeyCode::Enter));
    release.send(()).unwrap(); // the stale "a" scan
    assert_eq!(started.recv_timeout(SIGNAL_TIMEOUT).unwrap(), "ab");
    release.send(()).unwrap();
    let deadline = Instant::now() + SIGNAL_TIMEOUT;
    while controller.project_search_open() {
        controller.poll();
        assert!(
            Instant::now() < deadline,
            "the held Enter never opened the result"
        );
        std::thread::yield_now();
    }
    let selected = controller.tree().selected().expect("result selected");
    assert_eq!(
        selected.path.file_name().and_then(|n| n.to_str()),
        Some("ab.txt")
    );
}
