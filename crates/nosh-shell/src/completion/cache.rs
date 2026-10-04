use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::types::*;

const WORKER_CACHE_BYTES: usize = MAX_CACHE_BYTES - 2 * 1024 * 1024;

#[derive(Debug, Clone)]
pub(crate) struct Entry {
    pub value: String,
    pub kind: Kind,
    pub description: Option<String>,
}

pub(crate) struct Set {
    pub entries: Vec<Entry>,
    pub reason: Option<String>,
}

impl Set {
    pub fn lines<'a>(
        values: impl Iterator<Item = &'a str>,
        kind: Kind,
        description: Option<&str>,
    ) -> Self {
        Self::collect(values.map(|value| {
            Ok(Entry {
                value: value.into(),
                kind,
                description: description.map(str::to_owned),
            })
        }))
    }

    pub fn collect(values: impl Iterator<Item = Result<Entry, String>>) -> Self {
        let mut entries = Vec::new();
        let mut bytes = 0;
        let mut reason = None;
        for value in values {
            let entry = match value {
                Ok(entry) => entry,
                Err(error) => {
                    reason.get_or_insert(error);
                    continue;
                }
            };
            if entry.value.len() > MAX_WORD {
                reason.get_or_insert_with(|| "provider word limit reached".into());
                continue;
            }
            let size = entry.value.len() + entry.description.as_ref().map_or(0, String::len) + 96;
            if entries.len() >= MAX_SET || bytes + size > MAX_SET_BYTES {
                reason = Some("provider collection limit reached".into());
                break;
            }
            bytes += size;
            entries.push(entry);
        }
        entries.sort_by(|left, right| left.value.cmp(&right.value));
        entries.dedup_by(|left, right| left.value == right.value);
        Self { entries, reason }
    }

    pub fn bytes(&self) -> usize {
        self.entries
            .iter()
            .map(|entry| entry.value.len() + entry.description.as_ref().map_or(0, String::len) + 96)
            .sum()
    }
}

#[derive(Default)]
pub(crate) struct Cache {
    items: VecDeque<(String, Arc<Set>, Instant)>,
    bytes: usize,
}

impl Cache {
    pub fn load(
        &mut self,
        key: String,
        ttl: Duration,
        build: impl FnOnce() -> Result<Set, String>,
    ) -> Result<Arc<Set>, String> {
        if let Some(set) = self.get(&key, ttl) {
            return Ok(set);
        }
        Ok(self.insert(key, build()?))
    }

    pub fn get(&mut self, key: &str, ttl: Duration) -> Option<Arc<Set>> {
        let position = self
            .items
            .iter()
            .position(|(name, _, at)| name == key && at.elapsed() < ttl)?;
        let item = self.items.remove(position)?;
        let value = item.1.clone();
        self.items.push_back(item);
        Some(value)
    }

    pub fn insert(&mut self, key: String, set: Set) -> Arc<Set> {
        let set = Arc::new(set);
        let size = set.bytes() + key.len() + 64;
        if set.reason.is_none() && size <= MAX_SET_BYTES {
            while self.items.len() >= 128 || self.bytes + size > WORKER_CACHE_BYTES {
                let Some((key, old, _)) = self.items.pop_front() else {
                    break;
                };
                self.bytes -= old.bytes() + key.len() + 64;
            }
            if size <= WORKER_CACHE_BYTES {
                self.bytes += size;
                self.items.push_back((key, set.clone(), Instant::now()));
            }
        }
        set
    }
}
