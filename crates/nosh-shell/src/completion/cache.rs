use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::types::*;

const WORKER_CACHE_BYTES: usize = MAX_CACHE_BYTES * 3 / 8;

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
