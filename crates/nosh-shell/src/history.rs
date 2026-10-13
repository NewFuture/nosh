//! Shared command persistence and temporary question recall.

mod store;

pub(crate) use store::{Kind, Store};

use nosh_platform::tr;
use reedline::{
    HistoryPanelAnchor, HistoryPanelEntry, HistoryPanelPage, HistoryPanelProvider,
    HistoryPanelQuery,
};
use store::{Entry, Query};

pub(crate) const NOTICE: &str = "__nosh_history_notice__";

pub struct ShellHistory {
    pub(crate) store: Store,
}

fn item(entry: &Entry) -> reedline::HistoryItem {
    let mut result = reedline::HistoryItem::from_command_line(&entry.text);
    result.id = Some(reedline::HistoryItemId(entry.legacy_id));
    result.start_timestamp = entry
        .timestamp
        .and_then(chrono::DateTime::from_timestamp_micros);
    result.cwd = entry.cwd.clone();
    result.exit_status = entry.exit_code.map(i64::from);
    result
}

fn error(message: &'static str) -> reedline::ReedlineError {
    reedline::ReedlineError(reedline::ReedlineErrorVariants::OtherHistoryError(message))
}

impl reedline::History for ShellHistory {
    fn save(&mut self, value: reedline::HistoryItem) -> reedline::Result<reedline::HistoryItem> {
        // Only the classified host input is eligible for recording.
        Ok(value)
    }

    fn load(&self, id: reedline::HistoryItemId) -> reedline::Result<reedline::HistoryItem> {
        self.store
            .with_commands(|entries| {
                entries
                    .iter()
                    .find(|entry| entry.legacy_id == id.0)
                    .map(item)
            })
            .ok_or_else(|| error("history item is no longer available"))
    }

    fn count(&self, query: reedline::SearchQuery) -> reedline::Result<i64> {
        Ok(self.search(query)?.len() as i64)
    }

    fn search(&self, query: reedline::SearchQuery) -> reedline::Result<Vec<reedline::HistoryItem>> {
        let backward = query.direction == reedline::SearchDirection::Backward;
        let (earliest, latest) = if backward {
            (query.end_time, query.start_time)
        } else {
            (query.start_time, query.end_time)
        };
        let earliest = earliest.map(|time| time.timestamp_micros());
        let latest = latest.map(|time| time.timestamp_micros());
        let matches = |entry: &Entry| {
            query.filter.matches_command_line(&entry.text)
                && query.filter.hostname.is_none()
                && query.filter.exit_successful.is_none_or(|success| {
                    entry.exit_code.is_some_and(|code| (code == 0) == success)
                })
                && query.filter.session.is_none()
                && earliest.is_none_or(|time| entry.timestamp.is_some_and(|stamp| stamp > time))
                && latest.is_none_or(|time| entry.timestamp.is_some_and(|stamp| stamp < time))
                && query
                    .filter
                    .cwd_exact
                    .as_ref()
                    .is_none_or(|cwd| entry.cwd.as_ref() == Some(cwd))
                && query.filter.cwd_prefix.as_ref().is_none_or(|prefix| {
                    entry
                        .cwd
                        .as_ref()
                        .is_some_and(|cwd| cwd.starts_with(prefix))
                })
        };
        let result: Result<Vec<_>, ()> = self.store.with_commands(|entries| {
            let position = |id: Option<reedline::HistoryItemId>| {
                id.map(|id| {
                    entries
                        .iter()
                        .position(|entry| entry.legacy_id == id.0)
                        .ok_or(())
                })
                .transpose()
            };
            let start = position(query.start_id)?;
            let end = position(query.end_id)?;
            let (minimum, maximum) = if backward { (end, start) } else { (start, end) };
            let limit = query
                .limit
                .unwrap_or(entries.len() as i64)
                .clamp(0, entries.len() as i64) as usize;
            Ok((0..entries.len())
                .map(|index| {
                    if backward {
                        entries.len() - index - 1
                    } else {
                        index
                    }
                })
                .filter(|index| {
                    minimum.is_none_or(|minimum| *index > minimum)
                        && maximum.is_none_or(|maximum| *index < maximum)
                })
                .map(|index| &entries[index])
                .filter(|entry| matches(entry))
                .take(limit)
                .map(item)
                .collect())
        });
        match result {
            Ok(entries) => Ok(entries),
            Err(()) => {
                self.store.report(
                    "history changed while browsing; cancel recall or start a new search".into(),
                );
                Ok(Vec::new())
            }
        }
    }

    fn update(
        &mut self,
        id: reedline::HistoryItemId,
        updater: &dyn Fn(reedline::HistoryItem) -> reedline::HistoryItem,
    ) -> reedline::Result<()> {
        let updated = updater(self.load(id)?);
        brush_core::history::Observer::updated(
            &self.store,
            &brush_core::history::Item {
                id: id.0,
                command_line: updated.command_line,
                timestamp: updated.start_timestamp,
                dirty: true,
            },
        )
        .map_err(reedline::ReedlineError::from)
    }

    fn clear(&mut self) -> reedline::Result<()> {
        brush_core::history::Observer::cleared(&self.store).map_err(reedline::ReedlineError::from)
    }

    fn delete(&mut self, id: reedline::HistoryItemId) -> reedline::Result<()> {
        brush_core::history::Observer::removed(&self.store, id.0)
            .map_err(reedline::ReedlineError::from)
    }

    fn sync(&mut self) -> std::io::Result<()> {
        self.store.sync()
    }

    fn session(&self) -> Option<reedline::HistorySessionId> {
        None
    }
}

pub(crate) struct Panel {
    store: Store,
    current: Option<(HistoryPanelQuery, u64)>,
    snapshot: Option<i64>,
    initial: Option<(i64, String)>,
    preview: Option<store::Page>,
    revision: u64,
}

fn outcome_label(entry: &Entry) -> String {
    let repeat_count = entry.repeat_count();
    if repeat_count == 1 {
        return match entry.exit_code {
            Some(0) => tr!("成功", "ok").into(),
            Some(code) => tr!(format!("失败({code})"), format!("failed({code})")),
            None => tr!("未知", "unknown").into(),
        };
    }
    let counts = entry.outcomes();
    if counts.failed == repeat_count {
        return tr!("失败", "failed").into();
    }
    if counts.unknown == repeat_count {
        return tr!("未知", "unknown").into();
    }
    if counts.succeeded == repeat_count {
        return tr!("成功", "ok").into();
    }
    let mut parts = Vec::new();
    if counts.failed > 0 {
        parts.push(tr!(
            format!("失败{}", counts.failed),
            format!("fail:{}", counts.failed)
        ));
    }
    if counts.unknown > 0 {
        parts.push(tr!(
            format!("未知{}", counts.unknown),
            format!("unknown:{}", counts.unknown)
        ));
    }
    if counts.succeeded > 0 {
        parts.push(tr!(
            format!("成功{}", counts.succeeded),
            format!("ok:{}", counts.succeeded)
        ));
    }
    parts.join(" ")
}

impl Panel {
    pub(crate) fn new(store: Store) -> Self {
        Self {
            store,
            current: None,
            snapshot: None,
            initial: None,
            preview: None,
            revision: 0,
        }
    }

    fn entry(
        entry: Entry,
        scope: Result<Option<&str>, &str>,
        confirmed: bool,
    ) -> HistoryPanelEntry {
        let kind = match entry.kind {
            Kind::Command => "",
            Kind::Manual => tr!("手动", "manual"),
            Kind::Question => tr!("提问·内存", "question: memory"),
            Kind::SuggestionRequest => tr!("建议·内存", "suggest: memory"),
        };
        let mut labels = Vec::new();
        if confirmed && !entry.kind.is_question() {
            if entry.repeat_count() > 1 {
                labels.push(format!("x{}", entry.repeat_count()));
            }
            labels.push(outcome_label(&entry));
        }
        if !kind.is_empty() {
            labels.push(kind.into());
        }
        let location = match entry.worktree.as_deref() {
            Some(root) if scope == Ok(Some(root)) => String::new(),
            Some(root) => {
                let path = std::path::Path::new(root);
                let mut components = path
                    .components()
                    .rev()
                    .take(2)
                    .map(|part| part.as_os_str().to_string_lossy().into_owned())
                    .collect::<Vec<_>>();
                components.reverse();
                format!(
                    "{}:{}",
                    if scope.is_ok() {
                        tr!("其它", "other")
                    } else {
                        tr!("工作树", "worktree")
                    },
                    components.join(std::path::MAIN_SEPARATOR_STR)
                )
            }
            None => entry
                .cwd
                .clone()
                .unwrap_or_else(|| tr!("来源未知", "unknown location").into()),
        };
        HistoryPanelEntry {
            anchor: HistoryPanelAnchor {
                id: entry.key,
                order: entry.ordinal,
            },
            text: entry.text,
            label: labels.join(" "),
            source: crate::style::visible_text(&location).into_owned(),
            description: entry.cwd.as_ref().map(|cwd| {
                let path = std::path::Path::new(cwd);
                let display = scope
                    .ok()
                    .flatten()
                    .and_then(|root| path.strip_prefix(root).ok())
                    .map_or_else(
                        || cwd.clone(),
                        |relative| {
                            if relative.as_os_str().is_empty() {
                                tr!("项目根目录", "project root").into()
                            } else {
                                relative.display().to_string()
                            }
                        },
                    );
                format!(
                    "{}: {}",
                    tr!("输入目录", "Input directory"),
                    crate::style::visible_text(&display)
                )
            }),
        }
    }
}

impl HistoryPanelProvider for Panel {
    fn begin(&mut self) {
        self.store.cancel_query();
        self.current = None;
        self.snapshot = None;
        self.initial = self.store.latest_own_anchor();
        self.preview = None;
        self.revision = self.store.revision();
    }

    fn query(&mut self, request: &HistoryPanelQuery) -> HistoryPanelPage {
        let current_scope = self.store.scope();
        let scope = current_scope
            .as_ref()
            .map(|scope| scope.as_deref())
            .map_err(String::as_str);
        let mut scope_notice = current_scope.as_ref().err().cloned();
        let worktree = if request.project {
            match &current_scope {
                Ok(Some(worktree)) => Some(worktree.clone()),
                Ok(None) => {
                    scope_notice = Some(
                        tr!(
                            "同项目不可用：不在 Git 工作树中；当前仍为全部",
                            "Project scope unavailable outside a Git worktree; showing all"
                        )
                        .into(),
                    );
                    None
                }
                Err(_) => None,
            }
        } else {
            None
        };
        let title = format!(
            "{} [{}]",
            tr!("历史", "History"),
            if worktree.is_some() {
                tr!("同项目", "project")
            } else {
                tr!("全部", "all")
            }
        );
        let revision = self.store.revision();
        if self.revision != revision {
            self.snapshot = None;
            self.revision = revision;
            self.current = None;
        }
        if self.current.as_ref().is_none_or(|(old, _)| old != request) {
            let anchor = request
                .anchor
                .as_ref()
                .map(|anchor| (anchor.order, anchor.id.clone()));
            let initial = self.current.is_none() && request.text.is_empty() && anchor.is_none();
            let anchor = anchor.or_else(|| initial.then(|| self.initial.clone()).flatten());
            let query = Query {
                text: request.text.clone(),
                worktree,
                before: anchor,
                include_anchor: request.include_anchor || (initial && self.initial.is_some()),
                older: request.older,
                snapshot: self.snapshot,
            };
            self.preview = Some(self.store.preview(&query));
            let generation = self.store.query(query);
            self.current = Some((request.clone(), generation));
        }
        let generation = self.current.as_ref().unwrap().1;
        let ready = self.store.poll(generation);
        let provisional = ready.is_none();
        let page = ready.or_else(|| {
            self.preview
                .as_ref()
                .filter(|page| !page.entries.is_empty())
                .cloned()
        });
        let Some(page) = page else {
            return HistoryPanelPage {
                pending: true,
                title,
                progress: Some(tr!("正在查询历史...", "Searching history...").into()),
                notice: scope_notice,
                ..Default::default()
            };
        };
        if !provisional {
            self.snapshot = Some(page.snapshot);
        }
        let anchored =
            request.anchor.is_some() || (request.text.is_empty() && self.initial.is_some());
        let notice = scope_notice.or(page.notice);
        HistoryPanelPage {
            entries: page
                .entries
                .into_iter()
                .map(|entry| Self::entry(entry, scope, !provisional))
                .collect(),
            older: if request.older { page.more } else { anchored },
            newer: if request.older { anchored } else { page.more },
            pending: false,
            failed: page.failed,
            title,
            notice,
            progress: provisional.then(|| {
                tr!(
                    "正在查询更多历史；本地候选可选择",
                    "Searching more history; cached matches are selectable"
                )
                .into()
            }),
            preferred: self.initial.as_ref().map(|(_, key)| key.clone()),
        }
    }

    fn cancel(&mut self) {
        self.store.cancel_query();
        self.current = None;
    }
}
