//! When the user last did something, process-wide. Background work that
//! should wait for the user's own quiet hours (the role dreamer) reads this
//! instead of the machine's clock, which may keep another time zone.

use std::sync::atomic::{AtomicI64, Ordering};

use crate::timestamp::{TimestampMs, now_ms};

/// 0 until the first note.
static LAST_ACTIVE_AT: AtomicI64 = AtomicI64::new(0);

/// Note that the user is active now: they sent a message, or started the
/// backend.
pub fn note_user_activity() {
    LAST_ACTIVE_AT.fetch_max(now_ms(), Ordering::Relaxed);
}

/// How long the user has done nothing at `now` (ms); `now` itself when no
/// activity was ever noted.
pub fn user_quiet_for(now: TimestampMs) -> TimestampMs {
    now.saturating_sub(LAST_ACTIVE_AT.load(Ordering::Relaxed))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quiet_time_restarts_at_each_note() {
        note_user_activity();
        let now = now_ms();
        assert!(user_quiet_for(now) < 60_000);
        assert!(user_quiet_for(now + 3_600_000) >= 3_600_000);
    }
}
