//! What this process is doing right now.
//!
//! A hub that feels slow is either busy or broken, and from the outside those
//! look the same. These are the numbers that tell them apart: how many
//! transfers are in flight, how much this process has moved since it started,
//! and what it is costing the machine.
//!
//! All of it is in-process and resets when the process does. Nothing here is a
//! substitute for real monitoring; it is the answer to "what is it doing"
//! without having to attach anything.

use std::sync::atomic::{AtomicI64, AtomicU64, Ordering::Relaxed};
use std::sync::Mutex;
use std::time::Instant;

#[derive(Debug, Default, Clone, Copy, serde::Serialize)]
pub struct Moved {
    /// Completed transfers of this kind.
    pub count: u64,
    pub objects: u64,
    pub bytes: u64,
}

pub struct Live {
    /// Sync connections open right now. Signed, because a decrement that
    /// outruns its increment should show as the bug it is rather than
    /// wrapping to eighteen quintillion.
    open: AtomicI64,
    accepted: AtomicU64,
    push_count: AtomicU64,
    push_objects: AtomicU64,
    push_bytes: AtomicU64,
    pull_count: AtomicU64,
    pull_objects: AtomicU64,
    pull_bytes: AtomicU64,
    started: Instant,
    /// The last CPU sample, so a rate can be worked out between two asks.
    cpu: Mutex<Option<(Instant, f64)>>,
}

impl Default for Live {
    fn default() -> Self {
        Live {
            open: AtomicI64::new(0),
            accepted: AtomicU64::new(0),
            push_count: AtomicU64::new(0),
            push_objects: AtomicU64::new(0),
            push_bytes: AtomicU64::new(0),
            pull_count: AtomicU64::new(0),
            pull_objects: AtomicU64::new(0),
            pull_bytes: AtomicU64::new(0),
            started: Instant::now(),
            cpu: Mutex::new(None),
        }
    }
}

/// Decrements the open-session count however the session ends, including a
/// panic. Counting by hand at each exit is how a gauge drifts until it says
/// there are transfers running on an idle server.
pub struct Session<'a>(&'a Live);

impl Drop for Session<'_> {
    fn drop(&mut self) {
        self.0.open.fetch_sub(1, Relaxed);
    }
}

impl Live {
    pub fn session(&self) -> Session<'_> {
        self.open.fetch_add(1, Relaxed);
        self.accepted.fetch_add(1, Relaxed);
        Session(self)
    }

    pub fn pushed(&self, objects: u64, bytes: u64) {
        self.push_count.fetch_add(1, Relaxed);
        self.push_objects.fetch_add(objects, Relaxed);
        self.push_bytes.fetch_add(bytes, Relaxed);
    }

    pub fn pulled(&self, objects: u64, bytes: u64) {
        self.pull_count.fetch_add(1, Relaxed);
        self.pull_objects.fetch_add(objects, Relaxed);
        self.pull_bytes.fetch_add(bytes, Relaxed);
    }

    pub fn open_sessions(&self) -> i64 {
        self.open.load(Relaxed)
    }

    pub fn accepted(&self) -> u64 {
        self.accepted.load(Relaxed)
    }

    pub fn pushes(&self) -> Moved {
        Moved {
            count: self.push_count.load(Relaxed),
            objects: self.push_objects.load(Relaxed),
            bytes: self.push_bytes.load(Relaxed),
        }
    }

    pub fn pulls(&self) -> Moved {
        Moved {
            count: self.pull_count.load(Relaxed),
            objects: self.pull_objects.load(Relaxed),
            bytes: self.pull_bytes.load(Relaxed),
        }
    }

    pub fn uptime_secs(&self) -> u64 {
        self.started.elapsed().as_secs()
    }

    /// Share of one core this process has used since the last time this was
    /// asked. `None` on the first ask, because a rate needs two samples and
    /// reporting zero would read as "idle" rather than "not known yet".
    pub fn cpu_percent(&self) -> Option<f64> {
        let now = Instant::now();
        let used = process_cpu_secs()?;
        let mut last = self.cpu.lock().ok()?;
        let out = match *last {
            Some((then, before)) => {
                let wall = now.duration_since(then).as_secs_f64();
                (wall > 0.0).then(|| ((used - before) / wall * 100.0).max(0.0))
            }
            None => None,
        };
        *last = Some((now, used));
        out
    }
}

/// Seconds of CPU this process has used, user and system.
#[cfg(target_os = "linux")]
fn process_cpu_secs() -> Option<f64> {
    let stat = std::fs::read_to_string("/proc/self/stat").ok()?;
    // The command name can contain spaces and brackets, so fields are counted
    // from after the closing bracket rather than from the start.
    let rest = stat.rsplit_once(')').map(|(_, r)| r)?;
    let f: Vec<&str> = rest.split_whitespace().collect();
    // utime and stime are fields 14 and 15 overall; after the name they are
    // the 12th and 13th.
    let ticks: f64 = f.get(11)?.parse::<f64>().ok()? + f.get(12)?.parse::<f64>().ok()?;
    Some(ticks / 100.0)
}

#[cfg(not(target_os = "linux"))]
fn process_cpu_secs() -> Option<f64> {
    None
}

/// Resident memory of this process, in bytes.
#[cfg(target_os = "linux")]
pub fn process_rss() -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    for line in status.lines() {
        if let Some(v) = line.strip_prefix("VmRSS:") {
            return v.split_whitespace().next()?.parse::<u64>().ok().map(|kb| kb * 1024);
        }
    }
    None
}

#[cfg(not(target_os = "linux"))]
pub fn process_rss() -> Option<u64> {
    None
}

/// Total and available memory on this machine, in bytes.
#[cfg(target_os = "linux")]
pub fn system_memory() -> (Option<u64>, Option<u64>) {
    let Ok(info) = std::fs::read_to_string("/proc/meminfo") else {
        return (None, None);
    };
    let field = |name: &str| {
        info.lines()
            .find_map(|l| l.strip_prefix(name))
            .and_then(|v| v.split_whitespace().next())
            .and_then(|n| n.parse::<u64>().ok())
            .map(|kb| kb * 1024)
    };
    (field("MemTotal:"), field("MemAvailable:"))
}

#[cfg(not(target_os = "linux"))]
pub fn system_memory() -> (Option<u64>, Option<u64>) {
    (None, None)
}

/// One, five and fifteen minute load averages.
#[cfg(target_os = "linux")]
pub fn load_average() -> Option<[f64; 3]> {
    let s = std::fs::read_to_string("/proc/loadavg").ok()?;
    let f: Vec<f64> = s.split_whitespace().take(3).filter_map(|n| n.parse().ok()).collect();
    (f.len() == 3).then(|| [f[0], f[1], f[2]])
}

#[cfg(not(target_os = "linux"))]
pub fn load_average() -> Option<[f64; 3]> {
    None
}
