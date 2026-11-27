use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

use chrono::{DateTime, TimeZone};
use chrono_tz::Tz;

/// Unix timestamp in milliseconds.
pub type TimestampMs = i64;

/// Names the time zone the user lives in when it differs from the host's: a
/// machine kept on one zone can serve someone whose day runs on another.
const USER_TIME_ZONE_ENV: &str = "AIONUI_USER_TIMEZONE";

/// Get current timestamp in milliseconds.
pub fn now_ms() -> TimestampMs {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time before Unix epoch")
        .as_millis() as TimestampMs
}

/// The user's IANA time zone: the one `AIONUI_USER_TIMEZONE` names, else the
/// host's; `None` when neither names a zone the tz database knows.
pub fn user_time_zone_name() -> Option<&'static str> {
    static NAME: OnceLock<Option<String>> = OnceLock::new();
    NAME.get_or_init(|| {
        let configured = std::env::var(USER_TIME_ZONE_ENV).ok();
        let host = iana_time_zone::get_timezone().ok();
        pick_time_zone(configured.as_deref(), host.as_deref()).map(|tz| tz.name().to_owned())
    })
    .as_deref()
}

/// The user's time zone, UTC when none can be named.
pub fn user_time_zone() -> Tz {
    user_time_zone_name()
        .and_then(|name| name.parse().ok())
        .unwrap_or(Tz::UTC)
}

/// Now, on the user's clock.
pub fn user_now() -> DateTime<Tz> {
    chrono::Utc::now().with_timezone(&user_time_zone())
}

/// Today on the user's clock, `YYYY-MM-DD`.
pub fn user_today() -> String {
    user_now().format("%Y-%m-%d").to_string()
}

/// The user's day a millisecond timestamp falls on (`YYYY-MM-DD`), empty for
/// a timestamp that names no moment.
pub fn user_day(timestamp_ms: TimestampMs) -> String {
    format_in(&user_time_zone(), timestamp_ms, "%Y-%m-%d")
}

/// The minute of a millisecond timestamp on the user's clock
/// (`YYYY-MM-DD HH:MM`), empty for a timestamp that names no moment.
pub fn user_minute(timestamp_ms: TimestampMs) -> String {
    format_in(&user_time_zone(), timestamp_ms, "%Y-%m-%d %H:%M")
}

fn format_in(zone: &Tz, timestamp_ms: TimestampMs, pattern: &str) -> String {
    zone.timestamp_millis_opt(timestamp_ms)
        .single()
        .map(|moment| moment.format(pattern).to_string())
        .unwrap_or_default()
}

/// The configured zone when it is a real one, otherwise the host's.
fn pick_time_zone(configured: Option<&str>, host: Option<&str>) -> Option<Tz> {
    configured
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .and_then(|name| name.parse::<Tz>().ok())
        .or_else(|| host.and_then(|name| name.parse::<Tz>().ok()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_configured_zone_wins_over_the_host() {
        let tz = pick_time_zone(Some("Asia/Shanghai"), Some("America/New_York"));
        assert_eq!(tz, Some(chrono_tz::Asia::Shanghai));
    }

    #[test]
    fn a_blank_or_unknown_zone_falls_back_to_the_host() {
        assert_eq!(
            pick_time_zone(Some("  "), Some("America/New_York")),
            Some(chrono_tz::America::New_York)
        );
        assert_eq!(
            pick_time_zone(Some("Mars/Olympus"), Some("Europe/Paris")),
            Some(chrono_tz::Europe::Paris)
        );
        assert_eq!(pick_time_zone(None, Some("not a zone")), None);
    }

    #[test]
    fn a_moment_lands_on_the_day_of_the_zone_it_is_read_in() {
        // 2026-11-27 13:30 UTC: evening in Shanghai, morning in New York.
        let at = 1_795_786_200_000;
        assert_eq!(
            format_in(&chrono_tz::Asia::Shanghai, at, "%Y-%m-%d %H:%M"),
            "2026-11-27 21:30"
        );
        assert_eq!(
            format_in(&chrono_tz::America::New_York, at, "%Y-%m-%d %H:%M"),
            "2026-11-27 08:30"
        );
        // 2026-11-27 17:00 UTC: already the next day in Shanghai.
        assert_eq!(
            format_in(&chrono_tz::Asia::Shanghai, 1_795_798_800_000, "%Y-%m-%d"),
            "2026-11-28"
        );
    }

    #[test]
    fn test_now_ms_positive() {
        assert!(now_ms() > 0);
    }

    #[test]
    fn test_now_ms_reasonable_range() {
        let ts = now_ms();
        // After 2020-01-01 and before 2100-01-01
        assert!(ts > 1_577_836_800_000);
        assert!(ts < 4_102_444_800_000);
    }

    #[test]
    fn test_monotonic() {
        let ts1 = now_ms();
        let ts2 = now_ms();
        assert!(ts2 >= ts1);
    }
}
