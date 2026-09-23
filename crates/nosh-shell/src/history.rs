//! reedline history backed by the brush shell's own history (`HISTFILE`), so
//! the `history` builtin and the line editor see the same entries.

use std::sync::{Arc, Mutex, MutexGuard};

use crate::backend::BrushShell;

pub struct ShellHistory {
    pub shell: Arc<Mutex<BrushShell>>,
}

impl ShellHistory {
    fn lock(&self) -> MutexGuard<'_, BrushShell> {
        self.shell.lock().unwrap_or_else(|e| e.into_inner())
    }
}

fn unsupported(feature: &'static str) -> reedline::ReedlineError {
    reedline::ReedlineError(reedline::ReedlineErrorVariants::HistoryFeatureUnsupported {
        history: "nosh",
        feature,
    })
}

fn to_reedline(item: &brush_core::history::Item) -> reedline::HistoryItem {
    let mut rl = reedline::HistoryItem::from_command_line(item.command_line.as_str());
    rl.id = Some(reedline::HistoryItemId(item.id));
    rl.start_timestamp = item.timestamp;
    rl
}

fn to_query(q: reedline::SearchQuery) -> brush_core::history::Query {
    use brush_core::history::{CommandLineFilter, Direction, Query};
    let back = matches!(q.direction, reedline::SearchDirection::Backward);
    Query {
        direction: if back {
            Direction::Backward
        } else {
            Direction::Forward
        },
        max_items: q.limit,
        not_at_or_before_id: if back { q.end_id } else { q.start_id }.map(|i| i.0),
        not_at_or_after_id: if back { q.start_id } else { q.end_id }.map(|i| i.0),
        not_at_or_before_time: if back { q.end_time } else { q.start_time },
        not_at_or_after_time: if back { q.start_time } else { q.end_time },
        command_line_filter: q.filter.command_line.map(|c| match c {
            reedline::CommandLineSearch::Exact(s) => CommandLineFilter::Exact(s),
            reedline::CommandLineSearch::Substring(s) => CommandLineFilter::Contains(s),
            reedline::CommandLineSearch::Prefix(s) => CommandLineFilter::Prefix(s),
        }),
    }
}

impl reedline::History for ShellHistory {
    // Lines are added through the shell when they are run.
    fn save(&mut self, item: reedline::HistoryItem) -> reedline::Result<reedline::HistoryItem> {
        Ok(item)
    }

    fn load(&self, id: reedline::HistoryItemId) -> reedline::Result<reedline::HistoryItem> {
        let sh = self.lock();
        let h = sh.history().ok_or_else(|| unsupported("load"))?;
        match h.get_by_id(id.0) {
            Ok(Some(item)) => Ok(to_reedline(item)),
            _ => Err(reedline::ReedlineError(
                reedline::ReedlineErrorVariants::OtherHistoryError("history item not found"),
            )),
        }
    }

    fn count(&self, query: reedline::SearchQuery) -> reedline::Result<i64> {
        Ok(self.search(query)?.len() as i64)
    }

    fn search(&self, query: reedline::SearchQuery) -> reedline::Result<Vec<reedline::HistoryItem>> {
        let q = to_query(query);
        let sh = self.lock();
        let h = sh.history().ok_or_else(|| unsupported("search"))?;
        let items = h
            .search(q)
            .map_err(|e| reedline::ReedlineError::from(std::io::Error::other(e)))?
            .map(to_reedline)
            .collect();
        Ok(items)
    }

    fn update(
        &mut self,
        _id: reedline::HistoryItemId,
        _updater: &dyn Fn(reedline::HistoryItem) -> reedline::HistoryItem,
    ) -> reedline::Result<()> {
        Ok(())
    }

    fn clear(&mut self) -> reedline::Result<()> {
        let mut sh = self.lock();
        if let Some(h) = sh.history_mut() {
            h.clear()
                .map_err(|e| reedline::ReedlineError::from(std::io::Error::other(e)))?;
        }
        Ok(())
    }

    fn delete(&mut self, id: reedline::HistoryItemId) -> reedline::Result<()> {
        let mut sh = self.lock();
        if let Some(h) = sh.history_mut() {
            h.delete_item_by_id(id.0)
                .map_err(|e| reedline::ReedlineError::from(std::io::Error::other(e)))?;
        }
        Ok(())
    }

    fn sync(&mut self) -> std::io::Result<()> {
        self.lock().save_history().map_err(std::io::Error::other)
    }

    fn session(&self) -> Option<reedline::HistorySessionId> {
        None
    }
}
