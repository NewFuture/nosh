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

impl Entry {
    fn bytes(&self) -> usize {
        self.value.len() + self.description.as_ref().map_or(0, String::len) + 96
    }
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
            let size = entry.bytes();
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
        self.entries.iter().map(Entry::bytes).sum()
    }
}

struct CachedSet {
    key: String,
    set: Arc<Set>,
    at: Instant,
    bytes: usize,
}

#[derive(Default)]
pub(crate) struct Cache {
    items: VecDeque<CachedSet>,
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
        let position = self.items.iter().position(|item| item.key == key)?;
        let item = self.items.remove(position)?;
        if item.at.elapsed() >= ttl {
            self.bytes -= item.bytes;
            return None;
        }
        let value = item.set.clone();
        self.items.push_back(item);
        Some(value)
    }

    pub fn insert(&mut self, key: String, set: Set) -> Arc<Set> {
        if let Some(position) = self.items.iter().position(|item| item.key == key) {
            let old = self.items.remove(position).expect("cached key found");
            self.bytes -= old.bytes;
        }
        let set = Arc::new(set);
        let size = set.bytes() + key.len() + 64;
        if set.reason.is_none() && size <= MAX_SET_BYTES {
            while self.items.len() >= 128 || self.bytes + size > WORKER_CACHE_BYTES {
                let Some(old) = self.items.pop_front() else {
                    break;
                };
                self.bytes -= old.bytes;
            }
            if size <= WORKER_CACHE_BYTES {
                self.bytes += size;
                self.items.push_back(CachedSet {
                    key,
                    set: set.clone(),
                    at: Instant::now(),
                    bytes: size,
                });
            }
        }
        set
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(value: &str) -> Set {
        Set::lines(std::iter::once(value), Kind::Command, None)
    }

    #[test]
    fn replacement_keeps_only_the_latest_set_and_its_byte_cost() {
        let mut cache = Cache::default();
        let first = cache.insert("commands".into(), set("old"));
        let second = cache.insert("commands".into(), set("updated"));
        assert_eq!(cache.items.len(), 1);
        assert_eq!(cache.bytes, second.bytes() + "commands".len() + 64);
        let hit = cache.get("commands", Duration::from_secs(60)).unwrap();
        assert!(Arc::ptr_eq(&hit, &second));
        assert_eq!(first.entries[0].value, "old");
    }

    #[test]
    fn expired_sets_release_their_slot_and_accounted_bytes() {
        let mut cache = Cache::default();
        cache.insert("commands".into(), set("old"));
        assert!(cache.get("commands", Duration::ZERO).is_none());
        assert!(cache.items.is_empty());
        assert_eq!(cache.bytes, 0);
        for _ in 0..128 {
            let value = cache
                .load("commands".into(), Duration::ZERO, || Ok(set("updated")))
                .unwrap();
            assert_eq!(cache.items.len(), 1);
            assert_eq!(cache.bytes, value.bytes() + "commands".len() + 64);
        }
    }

    #[test]
    fn incomplete_replacement_does_not_leave_an_old_cached_result() {
        let mut cache = Cache::default();
        cache.insert("commands".into(), set("old"));
        let mut partial = set("new");
        partial.reason = Some("provider collection limit reached".into());
        let value = cache.insert("commands".into(), partial);
        assert_eq!(value.entries[0].value, "new");
        assert!(cache.get("commands", Duration::from_secs(60)).is_none());
        assert!(cache.items.is_empty());
        assert_eq!(cache.bytes, 0);
    }

    #[test]
    fn hits_keep_lru_order_and_eviction_respects_both_limits() {
        let mut cache = Cache::default();
        for i in 0..128 {
            cache.insert(i.to_string(), set("command"));
        }
        cache.get("0", Duration::from_secs(60)).unwrap();
        cache.insert("128".into(), set("command"));
        assert_eq!(cache.items.len(), 128);
        assert!(cache.get("1", Duration::from_secs(60)).is_none());
        assert!(cache.get("0", Duration::from_secs(60)).is_some());
        let key_bytes: usize = (0..=128)
            .filter(|i| *i != 1)
            .map(|i| i.to_string().len())
            .sum();
        assert_eq!(cache.bytes, 128 * (set("command").bytes() + 64) + key_bytes);

        let mut cache = Cache::default();
        for i in 0..4 {
            let value = Set::collect(std::iter::once(Ok(Entry {
                value: "command".into(),
                kind: Kind::Command,
                description: Some("x".repeat(2 * 1024 * 1024)),
            })));
            cache.insert(i.to_string(), value);
            assert!(cache.bytes <= WORKER_CACHE_BYTES);
        }
        assert_eq!(cache.items.len(), 2);
        assert!(cache.get("0", Duration::from_secs(60)).is_none());
        assert!(cache.get("3", Duration::from_secs(60)).is_some());
    }
}
