use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Direction, Line, Point};
use alacritty_terminal::term::Term;
use alacritty_terminal::term::search::{Match, RegexIter, RegexSearch};
use alacritty_terminal::term::{TermMode, cell::Flags};
use std::sync::Arc;

const MAX_QUERY_BYTES: usize = 256;
const MAX_CELLS: usize = 250_000;
const MAX_MATCHES: usize = 2000;

#[derive(Clone, Copy, PartialEq, Eq)]
struct SearchStamp {
    revision: u64,
    columns: usize,
    screen_lines: usize,
    total_lines: usize,
    alternate: bool,
}

#[derive(Clone, PartialEq, Eq)]
struct MatchAnchor {
    occurrence: usize,
    text: Arc<str>,
}

#[derive(Default)]
pub struct OutputSearch {
    pub open: bool,
    pub query: String,
    pub error: String,
    pub selected: Option<usize>,
    pub rows: usize,
    pub limited: bool,
    pub more_matches: bool,
    pub bar_at_bottom: bool,
    pub editing: bool,
    regex: Option<RegexSearch>,
    matches: Vec<Match>,
    reveal: bool,
    stamp: Option<SearchStamp>,
    anchors: Vec<Option<MatchAnchor>>,
    #[cfg(test)]
    scans: usize,
}

impl OutputSearch {
    pub fn set_query(&mut self, query: &str) {
        if self.query == query {
            return;
        }
        self.query = query.to_owned();
        self.matches.clear();
        self.anchors.clear();
        self.stamp = None;
        self.selected = None;
        self.error.clear();
        self.reveal = true;
        self.regex = None;
        if query.len() > MAX_QUERY_BYTES {
            self.error = "Find is limited to 256 UTF-8 bytes.".into();
        } else if !query.is_empty() {
            let mut literal = String::from("(?-i:");
            for ch in query.chars() {
                if "\\.+*?()|[]{}^$".contains(ch) {
                    literal.push('\\');
                }
                literal.push(ch);
            }
            literal.push(')');
            match RegexSearch::new(&literal) {
                Ok(regex) => self.regex = Some(regex),
                Err(error) => self.error = format!("Cannot search terminal: {error}"),
            }
        }
    }

    // The backend publishes PTY updates independently of repaint consumers.
    // Viewport scrolling and focus changes do not invalidate text matches.
    pub fn refresh<T: alacritty_terminal::event::EventListener>(
        &mut self,
        term: &mut Term<T>,
        revision: u64,
    ) -> bool {
        let Some(regex) = &mut self.regex else {
            return std::mem::take(&mut self.bar_at_bottom);
        };
        let previous_scope = (
            self.rows,
            self.limited,
            self.more_matches,
            self.bar_at_bottom,
            self.selected,
        );
        let stamp = SearchStamp {
            revision,
            columns: term.columns(),
            screen_lines: term.screen_lines(),
            total_lines: term.total_lines(),
            alternate: term.mode().contains(TermMode::ALT_SCREEN),
        };
        let mut changed = false;
        if self.stamp != Some(stamp) {
            #[cfg(test)]
            {
                self.scans += 1;
            }
            let anchor = self
                .selected
                .and_then(|index| self.anchors.get(index))
                .and_then(Option::as_ref)
                .filter(|anchor| {
                    self.anchors
                        .iter()
                        .flatten()
                        .filter(|a| *a == *anchor)
                        .count()
                        == 1
                })
                .cloned();
            self.rows = term.total_lines().min((MAX_CELLS / term.columns()).max(1));
            self.limited = self.rows < term.total_lines();
            let end = Point::new(
                Line(term.screen_lines() as i32 - self.rows as i32),
                Column(0),
            );
            let start = Point::new(term.bottommost_line(), term.last_column());
            let mut matches: Vec<_> = RegexIter::new(start, end, Direction::Left, term, regex)
                .take(MAX_MATCHES + 1)
                .collect();
            self.more_matches = matches.len() > MAX_MATCHES;
            matches.truncate(MAX_MATCHES);
            matches.reverse();
            let anchors = match_anchors(term, end.line, &matches, self.more_matches);
            self.selected = if self.stamp.is_none() || self.matches.is_empty() {
                matches.len().checked_sub(1)
            } else if self
                .stamp
                .is_some_and(|old| old.alternate == stamp.alternate)
            {
                anchor.and_then(|anchor| {
                    let mut found = anchors
                        .iter()
                        .enumerate()
                        .filter(|(_, other)| other.as_ref() == Some(&anchor));
                    let index = found.next()?.0;
                    found.next().is_none().then_some(index)
                })
            } else {
                None
            };
            changed = matches != self.matches;
            self.matches = matches;
            self.anchors = anchors;
            self.stamp = Some(stamp);
        }
        let reveal = std::mem::take(&mut self.reveal);
        if reveal && let Some(index) = self.selected {
            term.scroll_to_point(*self.matches[index].start());
        }
        if reveal || !self.editing {
            self.bar_at_bottom = self
                .selected
                .and_then(|index| self.matches.get(index))
                .is_some_and(|found| {
                    let middle = (i64::from(found.start().line.0) + i64::from(found.end().line.0))
                        / 2
                        + term.grid().display_offset() as i64;
                    (0..term.screen_lines() as i64 / 2).contains(&middle)
                });
        }
        changed
            || previous_scope
                != (
                    self.rows,
                    self.limited,
                    self.more_matches,
                    self.bar_at_bottom,
                    self.selected,
                )
    }

    pub fn navigate<T: alacritty_terminal::event::EventListener>(
        &mut self,
        term: &mut Term<T>,
        revision: u64,
        reverse: bool,
    ) {
        self.refresh(term, revision);
        let count = self.matches.len();
        if count != 0 {
            let index = self
                .selected
                .map_or(if reverse { count - 1 } else { 0 }, |index| {
                    (index + if reverse { count - 1 } else { 1 }) % count
                });
            self.selected = Some(index);
            self.reveal = true;
        }
    }

    pub fn highlight(&self, point: Point) -> Option<bool> {
        let index = self
            .matches
            .partition_point(|found| *found.start() <= point);
        let index = index.checked_sub(1)?;
        self.matches[index]
            .contains(&point)
            .then_some(self.selected == Some(index))
    }

    pub fn label(&self) -> String {
        if !self.error.is_empty() {
            self.error.clone()
        } else if self.query.is_empty() {
            "Literal / case-sensitive".into()
        } else if self.matches.is_empty() {
            "No results".into()
        } else if self.selected.is_none() {
            format!(
                "No active match / {}{}",
                self.matches.len(),
                if self.more_matches { "+" } else { "" }
            )
        } else {
            format!(
                "{} / {}{}",
                self.selected.map_or(0, |index| index + 1),
                self.matches.len(),
                if self.more_matches { "+" } else { "" },
            )
        }
    }
}

// Exact logical-line identity survives scrolling/reflow. Ambiguous identical
// lines, incomplete bounded lines and changed content deliberately lose selection.
fn match_anchors<T>(
    term: &Term<T>,
    first: Line,
    matches: &[Match],
    truncated: bool,
) -> Vec<Option<MatchAnchor>> {
    if matches.is_empty() {
        return Vec::new();
    }
    let mut groups = Vec::new();
    let mut start = first;
    for row in first.0..=term.bottommost_line().0 {
        let line = Line(row);
        if !term.grid()[line][term.last_column()]
            .flags
            .contains(Flags::WRAPLINE)
            || line == term.bottommost_line()
        {
            groups.push((start, line));
            start = line + 1;
        }
    }
    let mut group = 0;
    let mut text: Option<Arc<str>> = None;
    let mut occurrence = 0;
    matches
        .iter()
        .map(|found| {
            while groups[group].1 < found.start().line {
                group += 1;
                text = None;
                occurrence = 0;
            }
            let (start, end) = groups[group];
            let complete = (start == term.topmost_line()
                || !term.grid()[start - 1i32][term.last_column()]
                    .flags
                    .contains(Flags::WRAPLINE))
                && !(truncated && (start..=end).contains(&matches[0].start().line));
            let anchor = complete.then(|| MatchAnchor {
                occurrence,
                text: text
                    .get_or_insert_with(|| {
                        Arc::from(term.bounds_to_string(
                            Point::new(start, Column(0)),
                            Point::new(end, term.last_column()),
                        ))
                    })
                    .clone(),
            });
            occurrence += 1;
            anchor
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use alacritty_terminal::event::VoidListener;
    use alacritty_terminal::term::{Config, test::TermSize};
    use alacritty_terminal::vte::ansi::{Processor, StdSyncHandler};

    fn terminal(cols: usize, rows: usize, text: &str) -> Term<VoidListener> {
        let mut term = Term::new(Config::default(), &TermSize::new(cols, rows), VoidListener);
        Processor::<StdSyncHandler>::new().advance(&mut term, text.as_bytes());
        term
    }

    #[test]
    fn finds_literal_wide_text_across_soft_wrap_but_not_hard_newline() {
        let mut term = terminal(6, 4, "ab中文cd\r\nab中\r\n文cd");
        let mut search = OutputSearch::default();
        search.set_query("中文cd");
        search.refresh(&mut term, 0);
        assert_eq!(search.matches.len(), 1);
        assert_eq!(search.matches[0].start(), &Point::new(Line(0), Column(2)));
        assert_eq!(search.matches[0].end(), &Point::new(Line(1), Column(1)));
        assert_eq!(search.highlight(Point::new(Line(0), Column(3))), Some(true));
        search.set_query("ab中文cd");
        search.refresh(&mut term, 0);
        assert_eq!(search.matches.len(), 1);
        search.set_query("AB");
        search.refresh(&mut term, 0);
        assert_eq!(search.label(), "No results");
        let mut term = terminal(20, 2, "a.b axb [x]");
        search.set_query("a.b");
        search.refresh(&mut term, 0);
        assert_eq!(search.matches.len(), 1);
        search.set_query("[x]");
        search.refresh(&mut term, 0);
        assert_eq!(search.matches.len(), 1);
    }

    #[test]
    fn refreshes_scrollback_and_alternate_screen_without_stale_positions() {
        let mut term = terminal(12, 2, "needle\r\nsecond\r\nthird");
        let mut search = OutputSearch::default();
        search.set_query("needle");
        search.refresh(&mut term, 0);
        assert_eq!(search.matches.len(), 1);
        assert!(search.matches[0].start().line.0 < 0);
        assert!(term.grid().display_offset() > 0);
        Processor::<StdSyncHandler>::new().advance(&mut term, b"\x1b[?1049hclean");
        search.refresh(&mut term, 1);
        assert!(search.matches.is_empty());
        Processor::<StdSyncHandler>::new().advance(&mut term, b"\x1b[?1049l");
        search.refresh(&mut term, 2);
        assert_eq!(search.matches.len(), 1);
        term.resize(TermSize::new(5, 2));
        search.refresh(&mut term, 2);
        assert_eq!(search.matches.len(), 1);
        search.navigate(&mut term, 2, true);
        assert_eq!(search.selected, Some(0));
    }

    #[test]
    fn keeps_find_controls_away_from_the_selected_match() {
        let mut term = terminal(16, 12, &format!("needle\r\n{}needle", "row\r\n".repeat(8)));
        let mut search = OutputSearch::default();
        search.set_query("needle");
        search.refresh(&mut term, 0);
        assert!(!search.bar_at_bottom);
        search.navigate(&mut term, 0, true);
        assert!(search.refresh(&mut term, 0));
        assert!(search.bar_at_bottom);
        search.navigate(&mut term, 0, false);
        assert!(search.refresh(&mut term, 0));
        assert!(!search.bar_at_bottom);
        search.navigate(&mut term, 0, true);
        search.refresh(&mut term, 0);
        search.set_query("");
        assert!(search.refresh(&mut term, 0));
        assert!(!search.bar_at_bottom);
    }

    #[test]
    fn bounds_result_count_and_query_size_and_keeps_sessions_independent() {
        let mut term = terminal(80, 40, &"x".repeat(3000));
        let mut search = OutputSearch::default();
        search.set_query("x");
        search.refresh(&mut term, 0);
        assert_eq!(search.matches.len(), MAX_MATCHES);
        assert!(search.more_matches);
        search.navigate(&mut term, 0, false);
        assert_eq!(search.selected, Some(0));
        Processor::<StdSyncHandler>::new().advance(&mut term, b"\r\nx new");
        search.refresh(&mut term, 1);
        assert_eq!(
            search.selected, None,
            "a truncated logical line has no stable occurrence index"
        );
        search.set_query(&"x".repeat(MAX_QUERY_BYTES + 1));
        search.refresh(&mut term, 0);
        assert!(!search.error.is_empty());
        assert!(search.matches.is_empty());
        let mut other = OutputSearch::default();
        other.set_query("absent");
        other.refresh(&mut term, 0);
        assert_eq!(other.label(), "No results");
        assert_ne!(other.query, search.query);
    }

    #[test]
    fn limits_scanned_cells_and_invalidates_erased_output() {
        let mut term = terminal(500, 2, &format!("old needle\r\n{}", "row\r\n".repeat(600)));
        let mut search = OutputSearch::default();
        search.set_query("needle");
        search.refresh(&mut term, 0);
        assert!(search.limited);
        assert!(search.rows * term.columns() <= MAX_CELLS);
        assert!(
            search.matches.is_empty(),
            "old rows are explicitly outside the bounded scope"
        );
        Processor::<StdSyncHandler>::new().advance(&mut term, b"\x1b[3J\x1b[2J\x1b[Hneedle");
        search.refresh(&mut term, 1);
        assert_eq!(search.matches.len(), 1);
        assert!(!search.limited);
        Processor::<StdSyncHandler>::new().advance(&mut term, b"\r\x1b[2K");
        search.refresh(&mut term, 2);
        assert!(search.matches.is_empty());
        assert_eq!(search.selected, None);
    }

    #[test]
    fn caches_unchanged_paints_and_navigation_and_preserves_selection_on_append() {
        let mut term = terminal(40, 12, "needle first\r\nneedle second");
        let mut search = OutputSearch::default();
        search.set_query("needle");
        search.refresh(&mut term, 0);
        for _ in 0..100 {
            search.navigate(&mut term, 0, true);
            search.refresh(&mut term, 0);
            term.scroll_display(alacritty_terminal::grid::Scroll::Bottom);
            search.refresh(&mut term, 0);
        }
        assert_eq!(search.scans, 1);
        search.navigate(&mut term, 0, true);
        assert_eq!(search.label(), "1 / 2");
        Processor::<StdSyncHandler>::new().advance(&mut term, b"\r\nneedle third");
        search.refresh(&mut term, 1);
        assert_eq!(search.label(), "1 / 3");
        assert_eq!(search.scans, 2);
        search.set_query("needle");
        search.refresh(&mut term, 1);
        assert_eq!(search.scans, 2);
        search.set_query("absent");
        search.refresh(&mut term, 1);
        for _ in 0..100 {
            search.refresh(&mut term, 1);
            search.navigate(&mut term, 1, false);
        }
        assert_eq!(search.scans, 3);
    }

    #[test]
    fn follows_unique_content_through_scrolling_and_reflow_but_not_eviction() {
        let mut term = terminal(20, 3, "needle first\r\nneedle second");
        term.set_options(Config {
            scrolling_history: 3,
            ..Config::default()
        });
        let mut search = OutputSearch::default();
        search.set_query("needle");
        search.refresh(&mut term, 0);
        search.navigate(&mut term, 0, true);
        Processor::<StdSyncHandler>::new().advance(&mut term, b"\r\nthird\r\nfourth");
        search.refresh(&mut term, 1);
        assert_eq!(search.label(), "1 / 2");
        assert!(search.matches[0].start().line.0 < 0);
        Processor::<StdSyncHandler>::new().advance(&mut term, b"\r\n5\r\n6\r\n7\r\nneedle tail");
        search.refresh(&mut term, 2);
        assert_eq!(search.label(), "No active match / 1");
        search.navigate(&mut term, 2, false);
        assert_eq!(search.label(), "1 / 1");

        let mut term = terminal(10, 12, "ab中文needle-tail\r\nother needle");
        search.set_query("needle-tail");
        search.refresh(&mut term, 0);
        assert_eq!(search.label(), "1 / 1");
        let scans = search.scans;
        term.resize(TermSize::new(5, 12));
        search.refresh(&mut term, 0);
        assert_eq!(search.scans, scans + 1);
        assert_eq!(search.label(), "1 / 1");
        term.resize(TermSize::new(24, 12));
        search.refresh(&mut term, 0);
        assert_eq!(search.label(), "1 / 1");
    }

    #[test]
    fn does_not_confuse_same_coordinates_replacement_duplicates_or_alternate_screen() {
        let mut term = terminal(40, 12, "needle first\r\nneedle second");
        let mut search = OutputSearch::default();
        search.set_query("needle");
        search.refresh(&mut term, 0);
        search.navigate(&mut term, 0, true);
        let old_position = search.matches[0].clone();
        Processor::<StdSyncHandler>::new().advance(&mut term, b"\x1b[H\x1b[2Kneedle replacement");
        assert!(search.refresh(&mut term, 1));
        assert_eq!(search.matches[0], old_position);
        assert_eq!(search.selected, None);
        search.navigate(&mut term, 1, false);
        assert_eq!(search.selected, Some(0));
        Processor::<StdSyncHandler>::new().advance(&mut term, b"\x1b[2J\x1b[3J\x1b[Hneedle new");
        search.refresh(&mut term, 2);
        assert_eq!(search.selected, None);
        search.navigate(&mut term, 2, false);
        Processor::<StdSyncHandler>::new().advance(&mut term, b"\x1b[?1049hneedle new");
        search.refresh(&mut term, 3);
        assert_eq!(search.selected, None);
        search.navigate(&mut term, 3, false);
        Processor::<StdSyncHandler>::new().advance(&mut term, b"\x1b[?1049l");
        search.refresh(&mut term, 4);
        assert_eq!(search.selected, None);

        let mut term = terminal(40, 12, "needle same\r\nneedle same");
        search = OutputSearch::default();
        search.set_query("needle");
        search.refresh(&mut term, 0);
        search.navigate(&mut term, 0, true);
        Processor::<StdSyncHandler>::new().advance(&mut term, b"\r\nunrelated output");
        search.refresh(&mut term, 1);
        assert_eq!(
            search.selected, None,
            "ambiguous identical lines must not be guessed"
        );
    }

    #[test]
    fn editing_keeps_overlay_stationary_until_explicit_navigation() {
        let mut term = terminal(20, 12, "needle first\r\nneedle second");
        let mut search = OutputSearch {
            editing: true,
            ..Default::default()
        };
        search.set_query("needle");
        search.refresh(&mut term, 0);
        search.navigate(&mut term, 0, true);
        search.refresh(&mut term, 0);
        assert!(search.bar_at_bottom);
        Processor::<StdSyncHandler>::new().advance(
            &mut term,
            b"\r\n3\r\n4\r\n5\r\n6\r\n7\r\n8\r\n9\r\n10\r\nneedle last",
        );
        search.refresh(&mut term, 1);
        assert_eq!(search.label(), "1 / 3");
        assert!(search.bar_at_bottom);
        search.navigate(&mut term, 1, true);
        search.refresh(&mut term, 1);
        assert_eq!(search.label(), "3 / 3");
        assert!(!search.bar_at_bottom);
        assert_eq!(search.scans, 2);
    }

    #[test]
    fn caches_the_large_no_match_case_and_refreshes_in_place_output() {
        let mut term = terminal(160, 40, &"ordinary log row\r\n".repeat(3000));
        let mut search = OutputSearch::default();
        search.set_query("needle");
        search.refresh(&mut term, 0);
        assert!(search.limited);
        let started = std::time::Instant::now();
        for _ in 0..100 {
            assert!(!search.refresh(&mut term, 0));
            search.navigate(&mut term, 0, false);
        }
        eprintln!(
            "100 cached large-grid refresh/navigation pairs: {:?}",
            started.elapsed()
        );
        assert_eq!(search.scans, 1);
        let dimensions = (term.columns(), term.screen_lines(), term.total_lines());
        Processor::<StdSyncHandler>::new().advance(&mut term, b"\r\x1b[2Kneedle");
        assert_eq!(
            dimensions,
            (term.columns(), term.screen_lines(), term.total_lines())
        );
        search.refresh(&mut term, 1);
        assert_eq!(search.label(), "1 / 1");
        assert_eq!(search.scans, 2);
        term.scroll_display(alacritty_terminal::grid::Scroll::Top);
        search.refresh(&mut term, 1);
        assert_eq!(search.scans, 2);
    }
}
