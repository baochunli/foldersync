//! Automatic scheduling and platform filesystem hints.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
    sync::{Arc, Mutex},
};

use chrono::{DateTime, TimeDelta, Utc};
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use sync_core::GenerationId;

pub(crate) const QUIET: TimeDelta = TimeDelta::seconds(2);
pub(crate) const COOLDOWN: TimeDelta = TimeDelta::seconds(30);
pub(crate) const MAX_RETRY_COUNT: u32 = 7;

#[derive(Debug)]
pub(crate) struct Schedule {
    pub generation: GenerationId,
    pub last_finished: DateTime<Utc>,
    pub retry_at: Option<DateTime<Utc>>,
    pub retry_count: u32,
    pub hint_at: Option<DateTime<Utc>>,
}

impl Schedule {
    pub fn new(generation: GenerationId, last_finished: DateTime<Utc>) -> Self {
        Self {
            generation,
            last_finished,
            retry_at: None,
            retry_count: 0,
            hint_at: None,
        }
    }

    pub fn next_poll(&self, interval_secs: u64) -> DateTime<Utc> {
        self.last_finished + TimeDelta::seconds(interval_secs.clamp(60, 86_400) as i64)
    }

    pub fn due(&self, now: DateTime<Utc>, interval_secs: u64) -> bool {
        // A network hint cannot distinguish a resolved outage from throttling,
        // which also uses retryable backoff.
        if let Some(retry_at) = self.retry_at {
            return now >= retry_at;
        }
        now >= self.next_poll(interval_secs)
            || (now >= self.last_finished + COOLDOWN
                && self.hint_at.is_some_and(|hint| now >= hint + QUIET))
    }

    pub fn finish(&mut self, now: DateTime<Utc>, retryable: bool, run_id: &str) {
        self.last_finished = now;
        self.retry_count = if retryable {
            (self.retry_count + 1).min(MAX_RETRY_COUNT)
        } else {
            0
        };
        self.retry_at = retryable.then(|| now + backoff(self.retry_count, run_id));
    }
}

/// Jitter is stable across restarts and spread by each run's random UUID.
pub(crate) fn backoff(attempt: u32, run_id: &str) -> TimeDelta {
    let base = (30 * (1 << attempt.saturating_sub(1).min(MAX_RETRY_COUNT - 1))).min(1500);
    let hash = run_id.bytes().fold(0_u64, |hash, byte| {
        hash.wrapping_mul(31).wrapping_add(u64::from(byte))
    });
    TimeDelta::seconds(base + (hash % (base as u64 / 5 + 1)) as i64)
}

/// A root watcher provides disposable hints. Interval runs remain the source of
/// reconciliation even when registration fails or the platform drops events.
pub trait LocalChangeWatcher: Send {
    fn set_roots(&mut self, roots: &BTreeSet<PathBuf>);
    fn take_changed_roots(&mut self) -> BTreeSet<PathBuf>;
}

#[derive(Default)]
struct PendingHints {
    roots: BTreeSet<PathBuf>,
    changed: BTreeSet<PathBuf>,
}

pub(crate) struct NativeWatcher {
    watcher: Option<RecommendedWatcher>,
    pending: Arc<Mutex<PendingHints>>,
    watched: BTreeSet<PathBuf>,
}

impl NativeWatcher {
    pub fn new() -> Self {
        let pending = Arc::new(Mutex::new(PendingHints::default()));
        let callback_state = Arc::clone(&pending);
        let watcher = notify::recommended_watcher(move |result: notify::Result<Event>| {
            let mut pending = callback_state.lock().unwrap();
            let affected: Vec<_> = match result {
                Ok(event) if event.need_rescan() => pending.roots.iter().cloned().collect(),
                Ok(event) if !matches!(event.kind, EventKind::Access(_)) => pending
                    .roots
                    .iter()
                    .filter(|root| event.paths.iter().any(|path| path.starts_with(root)))
                    .cloned()
                    .collect(),
                Err(_) => pending.roots.iter().cloned().collect(),
                _ => Vec::new(),
            };
            pending.changed.extend(affected);
        });
        Self {
            watcher: watcher
                .map_err(|err| eprintln!("File notifications unavailable: {err}"))
                .ok(),
            pending,
            watched: BTreeSet::new(),
        }
    }
}

impl LocalChangeWatcher for NativeWatcher {
    fn set_roots(&mut self, roots: &BTreeSet<PathBuf>) {
        let Some(watcher) = self.watcher.as_mut() else {
            return;
        };
        self.pending.lock().unwrap().roots.clone_from(roots);
        for root in self.watched.difference(roots) {
            let _ = watcher.unwatch(root);
        }
        self.watched.retain(|root| roots.contains(root));
        for root in roots.difference(&self.watched).cloned().collect::<Vec<_>>() {
            if watcher.watch(&root, RecursiveMode::Recursive).is_ok() {
                self.watched.insert(root);
            }
        }
    }

    fn take_changed_roots(&mut self) -> BTreeSet<PathBuf> {
        std::mem::take(&mut self.pending.lock().unwrap().changed)
    }
}

pub(crate) type NetworkAddresses = BTreeSet<(String, Vec<u8>)>;

/// Detect interface availability changes without contacting a remote service.
pub(crate) fn network_addresses() -> std::io::Result<NetworkAddresses> {
    let mut interfaces = std::ptr::null_mut();
    // SAFETY: getifaddrs initializes the head pointer on success and the list
    // remains valid until the matching freeifaddrs call.
    if unsafe { libc::getifaddrs(&mut interfaces) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    let mut addresses = BTreeSet::new();
    let mut entry = interfaces;
    while !entry.is_null() {
        // SAFETY: each entry and its non-null fields belong to the live list.
        let interface = unsafe { &*entry };
        entry = interface.ifa_next;
        let flags = interface.ifa_flags as libc::c_int;
        if flags & (libc::IFF_UP | libc::IFF_RUNNING) != (libc::IFF_UP | libc::IFF_RUNNING)
            || flags & libc::IFF_LOOPBACK != 0
            || interface.ifa_addr.is_null()
        {
            continue;
        }
        let bytes = unsafe {
            match i32::from((*interface.ifa_addr).sa_family) {
                libc::AF_INET => {
                    let address = (*interface.ifa_addr.cast::<libc::sockaddr_in>()).sin_addr;
                    address.s_addr.to_ne_bytes().to_vec()
                }
                libc::AF_INET6 => {
                    let address = (*interface.ifa_addr.cast::<libc::sockaddr_in6>()).sin6_addr;
                    if address.s6_addr[0] == 0xfe && address.s6_addr[1] & 0xc0 == 0x80 {
                        continue;
                    }
                    address.s6_addr.to_vec()
                }
                _ => continue,
            }
        };
        let name = unsafe { std::ffi::CStr::from_ptr(interface.ifa_name) };
        addresses.insert((name.to_string_lossy().into_owned(), bytes));
    }
    // SAFETY: the successful getifaddrs result is freed exactly once.
    unsafe { libc::freeifaddrs(interfaces) };
    Ok(addresses)
}

pub(crate) type Schedules = BTreeMap<sync_core::MappingId, Schedule>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_jitter_stays_within_twenty_percent_and_caps_at_thirty_minutes() {
        let mut initial = BTreeSet::new();
        for attempt in 1..=10 {
            let base = (30 * (1_i64 << (attempt - 1))).min(1500);
            for id in 0..50 {
                let delay = backoff(attempt, &format!("run-{id}")).num_seconds();
                assert!((base..=base + base / 5).contains(&delay));
                if attempt == 1 {
                    initial.insert(delay);
                }
            }
        }
        assert!(initial.len() > 1);
    }
}
