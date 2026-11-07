//! Volatile storage primitive for one-time handoffs.
//!
//! This module does not authenticate callers or implement the handoff API.
//! Callers must serialize its operations with their own lifecycle checks and
//! arrange periodic expiry even when no requests arrive.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Instant;

const MAX_REQUESTS: usize = 100;
const MAX_BYTES: usize = 1024 * 1024;
const MAX_VALUE: usize = 16 * 1024;

/// A buffer that is overwritten on drop on a best-effort basis. This is not a
/// guarantee against compiler optimizations, swap, dumps, or prior copies.
pub struct Secret(Vec<u8>);

impl Secret {
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

impl Drop for Secret {
    fn drop(&mut self) {
        self.0.fill(0);
    }
}

struct Entry {
    deadline: Instant,
    value: Option<Secret>,
}

#[derive(Default)]
struct Inner {
    entries: HashMap<String, Entry>,
    bytes: usize,
}

/// Errors deliberately contain neither submitted values nor their lengths.
#[derive(Debug, PartialEq, Eq)]
pub enum StoreError {
    Capacity,
    Duplicate,
    NotFound,
    InvalidValue,
    NotReady,
}

/// Process-local storage. An entry reserves a request slot before submission.
#[derive(Default)]
pub struct SecretMemory(Mutex<Inner>, tokio::sync::Notify);

impl SecretMemory {
    pub fn reserve(&self, id: &str, deadline: Instant, now: Instant) -> Result<(), StoreError> {
        let mut inner = self.0.lock().unwrap_or_else(|e| e.into_inner());
        inner.expire(now);
        if deadline <= now {
            return Err(StoreError::NotFound);
        }
        if inner.entries.contains_key(id) {
            return Err(StoreError::Duplicate);
        }
        if inner.entries.len() >= MAX_REQUESTS {
            return Err(StoreError::Capacity);
        }
        inner.entries.insert(
            id.to_owned(),
            Entry {
                deadline,
                value: None,
            },
        );
        self.1.notify_one();
        Ok(())
    }

    pub fn submit(
        &self,
        id: &str,
        value: String,
        deadline: Instant,
        now: Instant,
    ) -> Result<(), StoreError> {
        let value = Secret(value.into_bytes());
        if value.0.is_empty() || value.0.len() > MAX_VALUE || value.0.contains(&0) {
            return Err(StoreError::InvalidValue);
        }
        let mut inner = self.0.lock().unwrap_or_else(|e| e.into_inner());
        inner.expire(now);
        let entry = inner.entries.get(id).ok_or(StoreError::NotFound)?;
        if entry.value.is_some() {
            return Err(StoreError::Duplicate);
        }
        if deadline <= now {
            return Err(StoreError::NotFound);
        }
        if inner.bytes + value.0.len() > MAX_BYTES {
            return Err(StoreError::Capacity);
        }
        inner.bytes += value.0.len();
        let entry = inner.entries.get_mut(id).expect("entry checked above");
        entry.deadline = entry.deadline.min(deadline);
        entry.value = Some(value);
        self.1.notify_one();
        Ok(())
    }

    /// Consume at most once. A waiting request is not consumed.
    pub fn take(&self, id: &str, now: Instant) -> Result<Secret, StoreError> {
        let mut inner = self.0.lock().unwrap_or_else(|e| e.into_inner());
        inner.expire(now);
        match inner.entries.get(id) {
            None => return Err(StoreError::NotFound),
            Some(entry) if entry.value.is_none() => return Err(StoreError::NotReady),
            Some(_) => {}
        }
        let value = inner
            .entries
            .remove(id)
            .and_then(|entry| entry.value)
            .expect("ready entry");
        inner.bytes -= value.0.len();
        Ok(value)
    }

    pub fn remove(&self, id: &str) {
        let mut inner = self.0.lock().unwrap_or_else(|e| e.into_inner());
        inner.remove(id);
    }

    pub fn expire(&self, now: Instant) -> Vec<String> {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).expire(now)
    }

    pub fn next_deadline(&self) -> Option<Instant> {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entries
            .values()
            .map(|entry| entry.deadline)
            .min()
    }

    pub async fn changed(&self) {
        self.1.notified().await;
    }

    /// Remove entries no longer live in the serialized database state.
    pub fn retain_live(&self, live: &std::collections::HashSet<String>) -> Vec<String> {
        let mut inner = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let removed: Vec<_> = inner
            .entries
            .keys()
            .filter(|id| !live.contains(*id))
            .cloned()
            .collect();
        for id in &removed {
            inner.remove(id);
        }
        if !removed.is_empty() {
            self.1.notify_one();
        }
        removed
    }
}

impl Inner {
    fn remove(&mut self, id: &str) {
        if let Some(Entry {
            value: Some(value), ..
        }) = self.entries.remove(id)
        {
            self.bytes -= value.0.len();
        }
    }

    fn expire(&mut self, now: Instant) -> Vec<String> {
        let mut removed = Vec::new();
        self.entries.retain(|id, entry| {
            if entry.deadline <= now {
                removed.push(id.clone());
                if let Some(value) = &entry.value {
                    self.bytes -= value.0.len();
                }
                false
            } else {
                true
            }
        });
        removed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::time::Duration;

    #[test]
    fn exact_value_and_one_time_concurrent_take() {
        let store = Arc::new(SecretMemory::default());
        let now = Instant::now();
        let deadline = now + Duration::from_secs(60);
        store.reserve("r", deadline, now).unwrap();
        store
            .submit("r", " \nvalue\n ".into(), deadline, now)
            .unwrap();
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let store = Arc::clone(&store);
                std::thread::spawn(move || store.take("r", now).ok().map(|s| s.as_bytes().to_vec()))
            })
            .collect();
        let values: Vec<_> = threads
            .into_iter()
            .filter_map(|t| t.join().unwrap())
            .collect();
        assert_eq!(values, vec![b" \nvalue\n ".to_vec()]);
    }

    #[test]
    fn expiry_validation_and_duplicate_preserve_original() {
        let store = SecretMemory::default();
        let now = Instant::now();
        let deadline = now + Duration::from_secs(60);
        store.reserve("r", deadline, now).unwrap();
        assert_eq!(
            store.submit("r", "\0".into(), deadline, now),
            Err(StoreError::InvalidValue)
        );
        store.submit("r", "original".into(), deadline, now).unwrap();
        assert_eq!(
            store.submit("r", "replacement".into(), deadline, now),
            Err(StoreError::Duplicate)
        );
        assert_eq!(store.take("r", now).unwrap().as_bytes(), b"original");
        store.reserve("r", deadline, now).unwrap();
        store.submit("r", "value".into(), deadline, now).unwrap();
        assert!(matches!(
            store.take("r", deadline),
            Err(StoreError::NotFound)
        ));
    }

    #[test]
    fn request_and_byte_limits_do_not_replace_existing_values() {
        let store = SecretMemory::default();
        let now = Instant::now();
        let deadline = now + Duration::from_secs(60);
        for i in 0..MAX_REQUESTS {
            store.reserve(&i.to_string(), deadline, now).unwrap();
        }
        assert_eq!(
            store.reserve("extra", deadline, now),
            Err(StoreError::Capacity)
        );
        for i in 0..64 {
            store
                .submit(&i.to_string(), "x".repeat(MAX_VALUE), deadline, now)
                .unwrap();
        }
        assert_eq!(
            store.submit("64", "x".into(), deadline, now),
            Err(StoreError::Capacity)
        );
        store.remove("0");
        store.submit("64", "x".into(), deadline, now).unwrap();
        assert_eq!(
            store.take("1", now).unwrap().as_bytes(),
            "x".repeat(MAX_VALUE).as_bytes()
        );
    }
}
