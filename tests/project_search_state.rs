use herdr_file_viewer::project_search::ProjectSearchState;
use herdr_file_viewer::repo_search::{SearchHit, SearchOutput};

fn hit(path: &str, line: usize) -> SearchHit {
    SearchHit {
        path: path.into(),
        line,
        column: 1,
        excerpt: "needle".into(),
    }
}

#[test]
fn editing_clears_stale_results_and_marks_nonempty_query_searching() {
    let mut state = ProjectSearchState::new(false);
    state.apply(
        SearchOutput {
            hits: vec![hit("a.txt", 1)],
            limited: true,
        },
        true,
    );
    state.move_selection(1);
    state.scroll_right();

    state.push('n');

    assert_eq!(state.query(), "n");
    assert!(state.hits().is_empty());
    assert_eq!(state.cursor(), 0);
    assert_eq!(state.hscroll(), 0);
    assert!(state.searching());
    assert!(!state.limited());
}

#[test]
fn deleting_back_to_empty_query_returns_to_idle() {
    let mut state = ProjectSearchState::new(true);
    state.push('x');
    state.backspace();

    assert_eq!(state.query(), "");
    assert!(!state.searching());
    assert!(state.include_ignored());
}

#[test]
fn applying_results_clears_loading_and_exposes_selected_hit() {
    let mut state = ProjectSearchState::new(false);
    state.push('x');
    state.apply(
        SearchOutput {
            hits: vec![hit("a.txt", 2), hit("b.txt", 4)],
            limited: true,
        },
        true,
    );
    state.move_selection(1);

    assert!(!state.searching());
    assert!(state.limited());
    assert_eq!(
        state.selected().map(|h| (&h.path, h.line)),
        Some((&"b.txt".to_string(), 4))
    );
}

#[test]
fn selection_and_horizontal_scroll_are_clamped() {
    let mut state = ProjectSearchState::new(false);
    state.apply(
        SearchOutput {
            hits: vec![hit("a.txt", 1), hit("b.txt", 2)],
            limited: false,
        },
        true,
    );

    state.move_selection(99);
    assert_eq!(state.cursor(), 1);
    state.move_selection(-99);
    assert_eq!(state.cursor(), 0);

    state.scroll_left();
    assert_eq!(state.hscroll(), 0);
    state.scroll_right();
    assert_eq!(state.hscroll(), 8);
    state.clamp_hscroll(3);
    assert_eq!(state.hscroll(), 3);
}

#[test]
fn partial_results_keep_searching_and_preserve_the_selection() {
    let mut state = ProjectSearchState::new(false);
    state.push('n');
    state.apply(
        SearchOutput {
            hits: vec![hit("a.txt", 1), hit("b.txt", 2)],
            limited: false,
        },
        false,
    );
    state.move_selection(1);
    assert!(state.searching());
    assert_eq!(state.status().as_deref(), Some("Searching… 2 matches"));

    // A later partial only appends, so the selected row is still the same hit.
    state.apply(
        SearchOutput {
            hits: vec![hit("a.txt", 1), hit("b.txt", 2), hit("c.txt", 3)],
            limited: false,
        },
        false,
    );
    assert_eq!(state.selected().map(|h| h.path.as_str()), Some("b.txt"));

    state.apply(
        SearchOutput {
            hits: vec![hit("a.txt", 1), hit("b.txt", 2), hit("c.txt", 3)],
            limited: false,
        },
        true,
    );
    assert!(!state.searching());
    assert_eq!(state.status().as_deref(), Some("3 matches"));
}

#[test]
fn status_reports_progress_then_outcome() {
    let mut state = ProjectSearchState::new(false);
    assert_eq!(state.status(), None, "no status before anything is typed");
    state.push('n');
    assert_eq!(state.status().as_deref(), Some("Searching…"));
    state.apply(SearchOutput::default(), true);
    assert_eq!(state.status().as_deref(), Some("No matches"));
    state.apply(
        SearchOutput {
            hits: vec![hit("a.txt", 1)],
            limited: true,
        },
        true,
    );
    assert_eq!(state.status().as_deref(), Some("1+ matches"));
}

#[test]
fn a_held_enter_resolves_on_the_first_hit_or_an_empty_finish() {
    let mut state = ProjectSearchState::new(false);
    assert!(
        !state.request_confirm(),
        "nothing typed: nothing to wait for"
    );
    state.push('n');
    assert!(state.request_confirm());
    assert_eq!(state.take_pending_confirm(), None, "still searching");
    state.apply(SearchOutput::default(), true);
    assert_eq!(
        state.take_pending_confirm(),
        Some(false),
        "finished empty: drop it"
    );

    state.push('e');
    assert!(state.request_confirm());
    state.apply(
        SearchOutput {
            hits: vec![hit("a.txt", 1)],
            limited: false,
        },
        false,
    );
    assert_eq!(
        state.take_pending_confirm(),
        Some(true),
        "a partial hit is enough"
    );
    assert_eq!(state.take_pending_confirm(), None, "resolved once");

    state.push('x');
    assert!(state.request_confirm());
    state.push('y');
    state.apply(SearchOutput::default(), true);
    assert_eq!(
        state.take_pending_confirm(),
        None,
        "an edit discards the held Enter"
    );
}
