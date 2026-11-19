use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Direction, Line, Point};
use alacritty_terminal::term::Term;
use alacritty_terminal::term::search::{Match, RegexIter, RegexSearch};

const MAX_QUERY_BYTES: usize = 256;
const MAX_CELLS: usize = 250_000;
const MAX_MATCHES: usize = 2000;

#[derive(Default)]
pub struct OutputSearch {
    pub open: bool,
    pub query: String,
    pub error: String,
    pub selected: Option<usize>,
    pub rows: usize,
    pub limited: bool,
    pub more_matches: bool,
    regex: Option<RegexSearch>,
    matches: Vec<Match>,
    reveal: bool,
}

impl OutputSearch {
    pub fn set_query(&mut self, query: &str) {
        if self.query == query {
            return;
        }
        self.query = query.to_owned();
        self.matches.clear();
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

    // Rebuild under the same grid lock as painting/navigation: PTY scrolling,
    // reflow and alternate-screen changes must never leave stale cell locations.
    pub fn refresh<T: alacritty_terminal::event::EventListener>(
        &mut self,
        term: &mut Term<T>,
    ) -> bool {
        let Some(regex) = &mut self.regex else {
            return false;
        };
        let previous_scope = (self.rows, self.limited, self.more_matches);
        let rows = term.total_lines().min((MAX_CELLS / term.columns()).max(1));
        self.rows = rows;
        self.limited = rows < term.total_lines();
        let end = Point::new(Line(term.screen_lines() as i32 - rows as i32), Column(0));
        let start = Point::new(term.bottommost_line(), term.last_column());
        let mut matches: Vec<_> = RegexIter::new(start, end, Direction::Left, term, regex)
            .take(MAX_MATCHES + 1)
            .collect();
        self.more_matches = matches.len() > MAX_MATCHES;
        matches.truncate(MAX_MATCHES);
        matches.reverse();
        let changed = matches != self.matches;
        if changed {
            self.selected = matches.len().checked_sub(1);
            self.matches = matches;
        }
        if std::mem::take(&mut self.reveal)
            && let Some(index) = self.selected
        {
            term.scroll_to_point(*self.matches[index].start());
        }
        changed || previous_scope != (self.rows, self.limited, self.more_matches)
    }

    pub fn navigate<T: alacritty_terminal::event::EventListener>(
        &mut self,
        term: &mut Term<T>,
        reverse: bool,
    ) {
        self.refresh(term);
        let count = self.matches.len();
        if count != 0 {
            let index = (self.selected.unwrap_or(0) + if reverse { count - 1 } else { 1 }) % count;
            self.selected = Some(index);
            term.scroll_to_point(*self.matches[index].start());
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
        search.refresh(&mut term);
        assert_eq!(search.matches.len(), 1);
        assert_eq!(search.matches[0].start(), &Point::new(Line(0), Column(2)));
        assert_eq!(search.matches[0].end(), &Point::new(Line(1), Column(1)));
        assert_eq!(search.highlight(Point::new(Line(0), Column(3))), Some(true));
        search.set_query("ab中文cd");
        search.refresh(&mut term);
        assert_eq!(search.matches.len(), 1);
        search.set_query("AB");
        search.refresh(&mut term);
        assert_eq!(search.label(), "No results");
        let mut term = terminal(20, 2, "a.b axb [x]");
        search.set_query("a.b");
        search.refresh(&mut term);
        assert_eq!(search.matches.len(), 1);
        search.set_query("[x]");
        search.refresh(&mut term);
        assert_eq!(search.matches.len(), 1);
    }

    #[test]
    fn refreshes_scrollback_and_alternate_screen_without_stale_positions() {
        let mut term = terminal(12, 2, "needle\r\nsecond\r\nthird");
        let mut search = OutputSearch::default();
        search.set_query("needle");
        search.refresh(&mut term);
        assert_eq!(search.matches.len(), 1);
        assert!(search.matches[0].start().line.0 < 0);
        assert!(term.grid().display_offset() > 0);
        Processor::<StdSyncHandler>::new().advance(&mut term, b"\x1b[?1049hclean");
        search.refresh(&mut term);
        assert!(search.matches.is_empty());
        Processor::<StdSyncHandler>::new().advance(&mut term, b"\x1b[?1049l");
        search.refresh(&mut term);
        assert_eq!(search.matches.len(), 1);
        term.resize(TermSize::new(5, 2));
        search.refresh(&mut term);
        assert_eq!(search.matches.len(), 1);
        search.navigate(&mut term, true);
        assert_eq!(search.selected, Some(0));
    }

    #[test]
    fn bounds_result_count_and_query_size_and_keeps_sessions_independent() {
        let mut term = terminal(80, 40, &"x".repeat(3000));
        let mut search = OutputSearch::default();
        search.set_query("x");
        search.refresh(&mut term);
        assert_eq!(search.matches.len(), MAX_MATCHES);
        assert!(search.more_matches);
        search.navigate(&mut term, false);
        assert_eq!(search.selected, Some(0));
        search.set_query(&"x".repeat(MAX_QUERY_BYTES + 1));
        search.refresh(&mut term);
        assert!(!search.error.is_empty());
        assert!(search.matches.is_empty());
        let mut other = OutputSearch::default();
        other.set_query("absent");
        other.refresh(&mut term);
        assert_eq!(other.label(), "No results");
        assert_ne!(other.query, search.query);
    }

    #[test]
    fn limits_scanned_cells_and_invalidates_erased_output() {
        let mut term = terminal(500, 2, &format!("old needle\r\n{}", "row\r\n".repeat(600)));
        let mut search = OutputSearch::default();
        search.set_query("needle");
        search.refresh(&mut term);
        assert!(search.limited);
        assert!(search.rows * term.columns() <= MAX_CELLS);
        assert!(
            search.matches.is_empty(),
            "old rows are explicitly outside the bounded scope"
        );
        Processor::<StdSyncHandler>::new().advance(&mut term, b"\x1b[3J\x1b[2J\x1b[Hneedle");
        search.refresh(&mut term);
        assert_eq!(search.matches.len(), 1);
        assert!(!search.limited);
        Processor::<StdSyncHandler>::new().advance(&mut term, b"\r\x1b[2K");
        search.refresh(&mut term);
        assert!(search.matches.is_empty());
        assert_eq!(search.selected, None);
    }
}
