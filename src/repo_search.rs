//! Bounded, read-only project content search.
//!
//! The scanner walks the viewer root with the same Git-ignore policy as the file index, reads only
//! bounded UTF-8 text files, and returns one row per matching source line. It performs no writes and
//! degrades by skipping files that disappear, cannot be read, look binary, or exceed the size cap.
//! A scan checks its caller's cancellation probe between files and publishes the hits found so far
//! at most every [`PUBLISH_INTERVAL`], so a superseded query stops early and a slow one shows
//! results before it finishes.

use crate::{index, search};
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Files larger than this are skipped rather than read into memory.
pub const MAX_FILE_BYTES: u64 = 1024 * 1024;
/// Maximum number of matching-line rows retained for one query.
pub const MAX_RESULTS: usize = 500;
/// Maximum excerpt body width in Unicode scalar values, excluding edge ellipses.
pub const MAX_EXCERPT_CHARS: usize = 160;
/// Minimum time between partial-result publishes from one running scan.
pub const PUBLISH_INTERVAL: Duration = Duration::from_millis(50);
/// The head of a file read first to spot a binary (NUL byte) before reading the rest.
const SNIFF_BYTES: u64 = 8 * 1024;

/// One matching source line under the current viewer root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchHit {
    /// Root-relative, forward-slash path.
    pub path: String,
    /// One-based source line.
    pub line: usize,
    /// One-based Unicode-scalar column of the first occurrence on the line.
    pub column: usize,
    /// Bounded source-line context containing the first occurrence.
    pub excerpt: String,
}

/// Complete bounded output for one query.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SearchOutput {
    pub hits: Vec<SearchHit>,
    /// `true` when at least one additional matching line existed beyond [`MAX_RESULTS`].
    pub limited: bool,
}

/// One scan: the query and the scope it runs over.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchRequest {
    pub root: PathBuf,
    pub query: String,
    /// Bounds the ancestor `.gitignore` search at the repository ([`index::walk_builder`]).
    pub is_git_repo: bool,
    /// Mirrors the tree's `i` toggle.
    pub include_ignored: bool,
}

/// A running scan's link back to its caller.
pub struct SearchControl<'a> {
    /// Polled between files; once it returns `true` the scan stops and returns `None`.
    pub cancelled: &'a dyn Fn() -> bool,
    /// Receives the cumulative hits found so far while the scan is still running.
    pub partial: &'a dyn Fn(&SearchOutput),
}

/// A content searcher. Production uses [`search_with`]; tests inject gated fakes through
/// `Controller::set_project_searcher`. Returns `None` when cancelled.
pub type Searcher =
    Arc<dyn Fn(&SearchRequest, &SearchControl) -> Option<SearchOutput> + Send + Sync>;

/// Search text files under `root` to completion, using literal smartcase matching.
///
/// `include_ignored` mirrors the tree's `i` state and `is_git_repo` bounds the ancestor
/// `.gitignore` search at the repository, exactly as the file index does ([`index::file_walk`]).
/// The `.git` subtree is excluded in both modes.
pub fn search(root: &Path, query: &str, is_git_repo: bool, include_ignored: bool) -> SearchOutput {
    let request = SearchRequest {
        root: root.to_path_buf(),
        query: query.to_string(),
        is_git_repo,
        include_ignored,
    };
    let control = SearchControl {
        cancelled: &|| false,
        partial: &|_| {},
    };
    search_with(&request, &control).unwrap_or_default()
}

/// The production scan behind [`search`], with cancellation and partial results.
///
/// Files are visited in name order within each directory and lines in source order, so results
/// are deterministic and a partial result is always a prefix of the final one.
pub fn search_with(request: &SearchRequest, control: &SearchControl) -> Option<SearchOutput> {
    scan(request, control, PUBLISH_INTERVAL)
}

/// [`search_with`] with an explicit publish interval, so a unit test can force every publish.
fn scan(
    request: &SearchRequest,
    control: &SearchControl,
    publish_interval: Duration,
) -> Option<SearchOutput> {
    let mut output = SearchOutput::default();
    if request.query.is_empty() {
        return Some(output);
    }
    let (needle, case_sensitive) = search::smartcase_needle(&request.query);
    let root = request.root.as_path();
    let mut walk = index::file_walk(root, request.is_git_repo, request.include_ignored);
    walk.sort_by_file_name(|a, b| a.cmp(b));

    let mut published = 0;
    let mut last_publish = Instant::now();
    for entry in walk.build() {
        if (control.cancelled)() {
            return None;
        }
        // Publish at the top of every step, so hits found earlier still surface while the walk
        // skips a long run of binary, oversized, or unreadable entries.
        if output.hits.len() > published && last_publish.elapsed() >= publish_interval {
            (control.partial)(&output);
            published = output.hits.len();
            last_publish = Instant::now();
        }
        let Ok(entry) = entry else { continue };
        if !entry.file_type().is_some_and(|t| t.is_file())
            || entry.metadata().is_ok_and(|m| m.len() > MAX_FILE_BYTES)
        {
            continue;
        }
        let Ok(relative) = entry.path().strip_prefix(root) else {
            continue;
        };
        let Some(text) = read_bounded_text(entry.path()) else {
            continue;
        };
        let relative = index::rel_to_slash(relative);
        if scan_text(&relative, &text, &needle, case_sensitive, &mut output) {
            return Some(output); // the result cap is reached; nothing more can be shown
        }
    }
    Some(output)
}

/// Append one hit per matching line of `text`. Returns `true` once [`MAX_RESULTS`] is exceeded.
fn scan_text(
    path: &str,
    text: &str,
    needle: &str,
    case_sensitive: bool,
    output: &mut SearchOutput,
) -> bool {
    // ASCII folding keeps byte offsets identical, so a hit in the folded copy indexes the original.
    let folded;
    let haystack = if case_sensitive {
        text
    } else {
        folded = text.to_ascii_lowercase();
        folded.as_str()
    };
    if !haystack.contains(needle) {
        return false;
    }
    for (line_index, (line, folded_line)) in text.lines().zip(haystack.lines()).enumerate() {
        let Some(start) = folded_line.find(needle) else {
            continue;
        };
        if output.hits.len() == MAX_RESULTS {
            output.limited = true;
            return true;
        }
        output.hits.push(SearchHit {
            path: path.to_string(),
            line: line_index + 1,
            column: line[..start].chars().count() + 1,
            excerpt: excerpt_around(line, start, start + needle.len()),
        });
    }
    false
}

fn read_bounded_text(path: &Path) -> Option<String> {
    let mut file = File::open(path).ok()?;
    let mut bytes = Vec::new();
    // Sniff the head first so a binary file costs one small read, not up to MAX_FILE_BYTES.
    file.by_ref()
        .take(SNIFF_BYTES)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.contains(&0) {
        return None;
    }
    let rest = MAX_FILE_BYTES + 1 - bytes.len() as u64;
    file.take(rest).read_to_end(&mut bytes).ok()?;
    if bytes.len() as u64 > MAX_FILE_BYTES || bytes.contains(&0) {
        return None;
    }
    String::from_utf8(bytes).ok()
}

fn excerpt_around(line: &str, match_start: usize, match_end: usize) -> String {
    let total = line.chars().count();
    if total <= MAX_EXCERPT_CHARS {
        return line.trim().to_string();
    }

    let match_start_char = line[..match_start].chars().count();
    let match_len = line[match_start..match_end].chars().count();
    let before_budget = MAX_EXCERPT_CHARS.saturating_sub(match_len) / 2;
    let mut start = match_start_char.saturating_sub(before_budget);
    let mut end = (start + MAX_EXCERPT_CHARS).min(total);
    if end == total {
        start = end.saturating_sub(MAX_EXCERPT_CHARS);
    }
    // A query can itself exceed the normal excerpt budget. Keep the complete match visible rather
    // than returning a row whose context does not contain the text that produced it.
    if match_start_char + match_len > end {
        end = (match_start_char + match_len).min(total);
        start = end.saturating_sub(MAX_EXCERPT_CHARS.max(match_len));
    }

    let body: String = line.chars().skip(start).take(end - start).collect();
    let mut excerpt = String::new();
    if start > 0 {
        excerpt.push('…');
    }
    excerpt.push_str(body.trim());
    if end < total {
        excerpt.push('…');
    }
    excerpt
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn excerpt_keeps_a_long_match_whole() {
        let query = "needle".repeat(40);
        let line = format!("prefix {query} suffix");
        let start = "prefix ".len();
        let excerpt = excerpt_around(&line, start, start + query.len());
        assert!(excerpt.contains(&query));
    }

    /// The real scanner streams: with the interval forced to zero it publishes after each matching
    /// file, every partial is a strict prefix of the final result, and a binary entry between two
    /// matches does not hold back the earlier hit.
    #[test]
    fn the_scanner_publishes_growing_prefixes_of_its_final_result() {
        let root = std::env::temp_dir().join(format!(
            "hfv-repo-search-stream-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("a.txt"), "needle\n").unwrap();
        std::fs::write(root.join("b.bin"), b"\0binary").unwrap();
        std::fs::write(root.join("c.txt"), "needle\n").unwrap();
        std::fs::write(root.join("d.txt"), "needle\n").unwrap();

        let request = SearchRequest {
            root: root.clone(),
            query: "needle".into(),
            is_git_repo: false,
            include_ignored: false,
        };
        let partials = std::sync::Mutex::new(Vec::new());
        let partial = |out: &SearchOutput| partials.lock().unwrap().push(out.hits.clone());
        let control = SearchControl {
            cancelled: &|| false,
            partial: &partial,
        };
        let final_hits = scan(&request, &control, Duration::ZERO).unwrap().hits;
        let _ = std::fs::remove_dir_all(&root);

        let paths = |hits: &[SearchHit]| hits.iter().map(|h| h.path.clone()).collect::<Vec<_>>();
        assert_eq!(paths(&final_hits), ["a.txt", "c.txt", "d.txt"]);
        let partials = partials.into_inner().unwrap();
        assert_eq!(
            partials.iter().map(|p| paths(p)).collect::<Vec<_>>(),
            [vec!["a.txt"], vec!["a.txt", "c.txt"]],
            "a.txt surfaces at the binary entry, c.txt at d.txt; d.txt arrives with the final result"
        );
    }
}
