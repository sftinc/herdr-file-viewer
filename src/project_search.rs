//! Project-content search modal state.
//!
//! This module owns only ephemeral prompt/list interaction state. Filesystem work is delegated to
//! [`crate::repo_search`] and controller orchestration lives in `controller::project_search`.

use crate::prompt::PromptInput;
use crate::repo_search::{SearchHit, SearchOutput};

const HSCROLL_STEP: u16 = 8;

/// Mutable state held while the project-content search popup is open.
pub struct ProjectSearchState {
    prompt: PromptInput,
    hits: Vec<SearchHit>,
    cursor: usize,
    hscroll: u16,
    searching: bool,
    limited: bool,
    include_ignored: bool,
    /// Enter was pressed before the current query had any result; open its first hit on arrival.
    confirm_pending: bool,
}

impl ProjectSearchState {
    pub fn new(include_ignored: bool) -> Self {
        Self {
            prompt: PromptInput::new(),
            hits: Vec::new(),
            cursor: 0,
            hscroll: 0,
            searching: false,
            limited: false,
            include_ignored,
            confirm_pending: false,
        }
    }

    pub fn query(&self) -> &str {
        self.prompt.query()
    }

    pub fn hits(&self) -> &[SearchHit] {
        &self.hits
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    pub fn hscroll(&self) -> u16 {
        self.hscroll
    }

    pub fn searching(&self) -> bool {
        self.searching
    }

    pub fn limited(&self) -> bool {
        self.limited
    }

    pub fn include_ignored(&self) -> bool {
        self.include_ignored
    }

    /// The popup's top-right status chip: progress while a scan runs, then the outcome. `None`
    /// before anything has been typed.
    pub fn status(&self) -> Option<String> {
        if self.prompt.query().is_empty() {
            return None;
        }
        let n = self.hits.len();
        let count = if n == 1 {
            "1 match".to_string()
        } else {
            format!("{n} matches")
        };
        Some(if self.searching {
            if n == 0 {
                "Searching…".to_string()
            } else {
                format!("Searching… {count}")
            }
        } else if n == 0 {
            "No matches".to_string()
        } else if self.limited {
            format!("{n}+ matches")
        } else {
            count
        })
    }

    pub fn push(&mut self, c: char) {
        self.prompt.push(c);
        self.query_changed();
    }

    pub fn backspace(&mut self) {
        self.prompt.backspace();
        self.query_changed();
    }

    fn query_changed(&mut self) {
        self.hits.clear();
        self.cursor = 0;
        self.hscroll = 0;
        self.limited = false;
        self.confirm_pending = false;
        self.searching = !self.prompt.query().is_empty();
    }

    /// Show a scan's hits: the cumulative partial result while it runs (`done == false`), or the
    /// final one. Partial results only ever grow, so the selection stays on the same row.
    pub fn apply(&mut self, output: SearchOutput, done: bool) {
        self.hits = output.hits;
        self.limited = output.limited;
        self.searching = !done;
        self.cursor = self.cursor.min(self.hits.len().saturating_sub(1));
    }

    pub fn move_selection(&mut self, delta: isize) {
        if self.hits.is_empty() {
            self.cursor = 0;
            return;
        }
        let max = self.hits.len() as isize - 1;
        self.cursor = (self.cursor as isize + delta).clamp(0, max) as usize;
    }

    /// Remember an Enter pressed while the current query is still searching with no hits yet.
    /// Returns whether it was recorded (a finished or empty query has nothing to wait for).
    pub fn request_confirm(&mut self) -> bool {
        self.confirm_pending = self.searching && self.hits.is_empty();
        self.confirm_pending
    }

    /// Whether a pending Enter should now resolve: a hit has arrived to open, or the scan
    /// finished with none (the pending Enter is then dropped and the popup stays open).
    pub fn take_pending_confirm(&mut self) -> Option<bool> {
        if !self.confirm_pending || (self.hits.is_empty() && self.searching) {
            return None;
        }
        self.confirm_pending = false;
        Some(!self.hits.is_empty())
    }

    pub fn selected(&self) -> Option<&SearchHit> {
        self.hits.get(self.cursor)
    }

    pub fn scroll_left(&mut self) {
        self.hscroll = self.hscroll.saturating_sub(HSCROLL_STEP);
    }

    pub fn scroll_right(&mut self) {
        self.hscroll = self.hscroll.saturating_add(HSCROLL_STEP);
    }

    pub fn clamp_hscroll(&mut self, max: u16) {
        self.hscroll = self.hscroll.min(max);
    }
}
