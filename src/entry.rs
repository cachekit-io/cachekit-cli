//! What ck stores: the value envelope and the failure marker, and the rules
//! that judge them. Both are encrypted before they reach a backend.
//!
//! The formats are fixed-width big-endian fields. They carry no version byte:
//! the `v1` in the cache key is the version, and it is bound into the AAD.

use std::time::Duration;

/// The longest backoff step, and how far ahead a marker's `retry_at` may sit
/// before the marker is ignored as misdated.
pub const MAX_BACKOFF_MS: u64 = 15 * 60 * 1000;
const FIRST_BACKOFF_MS: u64 = 30 * 1000;

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

/// `{stored_at, stdout}` of one exit-0 run: `[stored_at ms: u64][stdout]`.
pub struct Envelope {
    pub stored_at_ms: u64,
    pub stdout: Vec<u8>,
}

impl Envelope {
    pub fn encode(stored_at_ms: u64, stdout: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(8 + stdout.len());
        out.extend_from_slice(&stored_at_ms.to_be_bytes());
        out.extend_from_slice(stdout);
        out
    }

    pub fn decode(mut bytes: Vec<u8>) -> Option<Self> {
        let stored_at_ms = u64::from_be_bytes(bytes.get(..8)?.try_into().ok()?);
        bytes.drain(..8);
        Some(Self {
            stored_at_ms,
            stdout: bytes,
        })
    }

    /// Judged on this reader's clock with this reader's flags, so lowering
    /// `--stale` takes effect on the next call whatever the writer used.
    pub fn freshness(&self, now_ms: u64, ttl_secs: u64, stale_secs: u64) -> Freshness {
        let age_ms = now_ms.saturating_sub(self.stored_at_ms);
        let ttl_ms = ttl_secs.saturating_mul(1000);
        let stale_ms = stale_secs.saturating_mul(1000);
        if age_ms < ttl_ms {
            Freshness::Fresh
        } else if age_ms < ttl_ms.saturating_add(stale_ms) {
            Freshness::Stale { age_ms }
        } else {
            Freshness::Expired
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Freshness {
    Fresh,
    Stale { age_ms: u64 },
    Expired,
}

/// `{exit, failures, retry_at}` of the latest failing run:
/// `[exit: i32][failures: u32][retry_at ms: u64]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Marker {
    pub exit: i32,
    pub failures: u32,
    pub retry_at_ms: u64,
}

impl Marker {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(16);
        out.extend_from_slice(&self.exit.to_be_bytes());
        out.extend_from_slice(&self.failures.to_be_bytes());
        out.extend_from_slice(&self.retry_at_ms.to_be_bytes());
        out
    }

    pub fn decode(bytes: &[u8]) -> Option<Self> {
        if bytes.len() != 16 {
            return None;
        }
        Some(Self {
            exit: i32::from_be_bytes(bytes[0..4].try_into().ok()?),
            failures: u32::from_be_bytes(bytes[4..8].try_into().ok()?),
            retry_at_ms: u64::from_be_bytes(bytes[8..16].try_into().ok()?),
        })
    }

    /// The marker after one more consecutive failure. The count survives
    /// between calls, so the backoff escalates: 30 s, 1 m, 2 m, ... 15 m.
    pub fn bumped(previous: Option<&Self>, exit: i32, now_ms: u64) -> Self {
        let failures = previous.map_or(1, |m| m.failures.saturating_add(1));
        let doublings = (failures - 1).min(16);
        let step = FIRST_BACKOFF_MS
            .saturating_mul(1 << doublings)
            .min(MAX_BACKOFF_MS);
        Self {
            exit,
            failures,
            retry_at_ms: now_ms.saturating_add(step),
        }
    }

    /// Active only while `now < retry_at <= now + 15 min` on this reader's
    /// clock. A marker dated further ahead (a fast writer clock, or a key
    /// holder writing a far-future marker) is ignored, so no reader is ever
    /// suppressed for longer than the cap.
    pub fn is_active(&self, now_ms: u64) -> bool {
        now_ms < self.retry_at_ms && self.retry_at_ms <= now_ms.saturating_add(MAX_BACKOFF_MS)
    }

    /// Long enough that the failure count survives until the next call.
    pub fn backend_ttl(&self, now_ms: u64) -> Duration {
        Duration::from_millis(
            self.retry_at_ms
                .saturating_sub(now_ms)
                .saturating_add(MAX_BACKOFF_MS),
        )
    }
}

/// `4m30s`-style rendering for stderr lines.
pub fn human(ms: u64) -> String {
    let secs = ms / 1000;
    let (d, h, m, s) = (secs / 86_400, secs / 3_600 % 24, secs / 60 % 60, secs % 60);
    match (d, h, m) {
        (0, 0, 0) => format!("{s}s"),
        (0, 0, _) if s == 0 => format!("{m}m"),
        (0, 0, _) => format!("{m}m{s}s"),
        (0, _, _) => format!("{h}h{m}m"),
        _ => format!("{d}d{h}h"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn envelope_round_trip_and_freshness() {
        let env = Envelope::decode(Envelope::encode(10_000, b"out")).unwrap();
        assert_eq!(
            (env.stored_at_ms, env.stdout.as_slice()),
            (10_000, &b"out"[..])
        );
        assert_eq!(env.freshness(10_999, 1, 0), Freshness::Fresh);
        assert_eq!(env.freshness(11_000, 1, 0), Freshness::Expired);
        assert_eq!(
            env.freshness(11_000, 1, 5),
            Freshness::Stale { age_ms: 1_000 }
        );
        assert_eq!(env.freshness(16_000, 1, 5), Freshness::Expired);
        // A writer clock ahead of the reader's reads as age 0, not negative.
        assert_eq!(env.freshness(5_000, 1, 0), Freshness::Fresh);
        assert!(Envelope::decode(vec![0; 7]).is_none());
    }

    #[test]
    fn backoff_steps() {
        let mut m = Marker::bumped(None, 1, 0);
        let mut steps = vec![m.retry_at_ms];
        for _ in 0..7 {
            m = Marker::bumped(Some(&m), 1, 0);
            steps.push(m.retry_at_ms);
        }
        let s = |n: u64| n * 1000;
        assert_eq!(
            steps,
            [s(30), s(60), s(120), s(240), s(480), s(900), s(900), s(900)]
        );
        assert_eq!(m.failures, 8);
        let far = Marker {
            failures: u32::MAX,
            ..m
        };
        assert_eq!(Marker::bumped(Some(&far), 1, 0).retry_at_ms, s(900));
    }

    #[test]
    fn active_window() {
        let m = Marker {
            exit: 1,
            failures: 1,
            retry_at_ms: 1_000_000,
        };
        assert!(m.is_active(999_999));
        assert!(!m.is_active(1_000_000));
        assert!(m.is_active(1_000_000 - MAX_BACKOFF_MS));
        assert!(!m.is_active(1_000_000 - MAX_BACKOFF_MS - 1));
        assert_eq!(Marker::decode(&m.encode()), Some(m));
        assert!(Marker::decode(&[0; 15]).is_none());
    }

    #[test]
    fn human_durations() {
        assert_eq!(human(5_400), "5s");
        assert_eq!(human(240_000), "4m");
        assert_eq!(human(270_000), "4m30s");
        assert_eq!(human(7_380_000), "2h3m");
        assert_eq!(human(90_000_000), "1d1h");
    }
}
