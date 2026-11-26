use std::str::FromStr;
use std::sync::Arc;

use chrono::{TimeZone, Utc};
use cron::Schedule;
use dashmap::DashMap;
use tokio::task::JoinHandle;

use aionui_common::{TimestampMs, now_ms};

use crate::error::CronError;
use crate::types::{CronJob, CronSchedule};

// ---------------------------------------------------------------------------
// Schedule validation
// ---------------------------------------------------------------------------

/// Normalize a cron expression so both 5-field (standard Unix) and 6-field
/// (seconds-prefixed, as required by the `cron` crate) forms are accepted.
/// A 5-field expression is promoted by prepending `0 ` for the seconds field.
pub(crate) fn normalize_cron_expr(expr: &str) -> String {
    let trimmed = expr.trim();
    let field_count = trimmed.split_whitespace().count();
    if field_count == 5 {
        format!("0 {trimmed}")
    } else {
        trimmed.to_owned()
    }
}

pub fn validate_cron_expression(expr: &str) -> Result<Schedule, CronError> {
    let normalized = normalize_cron_expr(expr);
    Schedule::from_str(&normalized).map_err(|e| CronError::InvalidCronExpression(format!("{expr}: {e}")))
}

pub fn validate_timezone(tz: &str) -> Result<chrono_tz::Tz, CronError> {
    tz.parse::<chrono_tz::Tz>()
        .map_err(|_| CronError::InvalidTimezone(tz.to_owned()))
}

// ---------------------------------------------------------------------------
// Next-run computation
// ---------------------------------------------------------------------------

pub fn compute_next_run(schedule: &CronSchedule, now: TimestampMs) -> Option<TimestampMs> {
    match schedule {
        CronSchedule::At { at_ms, .. } => Some(*at_ms),
        CronSchedule::Every { every_ms, .. } => {
            if *every_ms <= 0 {
                return None;
            }
            Some(now + *every_ms)
        }
        CronSchedule::Cron { expr, tz, .. } => compute_cron_next_run(expr, tz.as_deref(), now),
    }
}

pub fn compute_next_run_after_occurrence(
    schedule: &CronSchedule,
    scheduled_at: TimestampMs,
    now: TimestampMs,
) -> Option<TimestampMs> {
    match schedule {
        CronSchedule::At { .. } => None,
        CronSchedule::Every { every_ms, .. } => {
            if *every_ms <= 0 {
                return None;
            }
            let elapsed = now.saturating_sub(scheduled_at);
            let steps = elapsed.div_euclid(*every_ms) + 1;
            scheduled_at.checked_add(steps.checked_mul(*every_ms)?)
        }
        CronSchedule::Cron { expr, tz, .. } => compute_cron_next_run(expr, tz.as_deref(), now.max(scheduled_at)),
    }
}

fn compute_cron_next_run(expr: &str, tz: Option<&str>, now: TimestampMs) -> Option<TimestampMs> {
    let normalized = normalize_cron_expr(expr);
    let schedule = Schedule::from_str(&normalized).ok()?;

    if let Some(tz_str) = tz {
        let tz_parsed: chrono_tz::Tz = tz_str.parse().ok()?;
        let now_dt = tz_parsed.timestamp_millis_opt(now).single()?;
        let next = schedule.after(&now_dt).next()?;
        Some(next.timestamp_millis())
    } else {
        let now_dt = Utc.timestamp_millis_opt(now).single()?;
        let next = schedule.after(&now_dt).next()?;
        Some(next.timestamp_millis())
    }
}

// ---------------------------------------------------------------------------
// Runs that came due while the machine slept
// ---------------------------------------------------------------------------

/// The latest a run that came due during a sleep is still made on waking.
pub const CATCH_UP_LIMIT_MS: i64 = 2 * 60 * 60 * 1_000;

/// What to do on waking with a job whose run came due while the machine slept.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MissedRun {
    /// The latest occurrence is recent enough to be worth making: run it now.
    CatchUp { scheduled_at: TimestampMs },
    /// Too late for that, and the next run is near: let it come, unremarked.
    Pass,
    /// Too late to run and too long until the next: record it as missed.
    Missed,
}

/// Runs the latest occurrence that came due by `now` when it is late by no
/// more than half the job's interval, and never more than
/// [`CATCH_UP_LIMIT_MS`]; otherwise lets it pass when the next run is that
/// near anyway, and records it as missed only when the next run is far.
pub fn missed_run(schedule: &CronSchedule, due: TimestampMs, now: TimestampMs) -> MissedRun {
    let (latest, next) = match schedule {
        CronSchedule::At { at_ms, .. } => (*at_ms, None),
        CronSchedule::Every { every_ms, .. } if *every_ms > 0 => {
            let latest = due + now.saturating_sub(due).div_euclid(*every_ms) * every_ms;
            (latest, Some(latest + every_ms))
        }
        CronSchedule::Every { .. } => (due, None),
        CronSchedule::Cron { expr, tz, .. } => latest_cron_occurrence(expr, tz.as_deref(), due, now),
    };
    let grace = next.map_or(CATCH_UP_LIMIT_MS, |next| ((next - latest) / 2).min(CATCH_UP_LIMIT_MS));
    if now - latest <= grace {
        MissedRun::CatchUp { scheduled_at: latest }
    } else if next.is_some_and(|next| next - now <= CATCH_UP_LIMIT_MS) {
        MissedRun::Pass
    } else {
        MissedRun::Missed
    }
}

/// The latest occurrence of a cron schedule at or after `due` and no later
/// than `now`, and the one after it.
fn latest_cron_occurrence(
    expr: &str,
    tz: Option<&str>,
    due: TimestampMs,
    now: TimestampMs,
) -> (TimestampMs, Option<TimestampMs>) {
    let mut latest = due;
    // A week of minutes is the most a sleep is walked through.
    for _ in 0..10_080 {
        match compute_cron_next_run(expr, tz, latest) {
            Some(next) if next <= now => latest = next,
            next => return (latest, next),
        }
    }
    (latest, None)
}

/// Wall-clock time that passed beyond the timers' own clock between two looks,
/// over which the machine is taken to have slept.
const SLEEP_GAP_MS: i64 = 90_000;

/// Whether the machine slept between two looks. Timers wait on a monotonic
/// clock that stops while it sleeps; the wall clock does not.
pub fn slept_between(wall_elapsed_ms: i64, monotonic_elapsed_ms: i64) -> bool {
    wall_elapsed_ms - monotonic_elapsed_ms > SLEEP_GAP_MS
}

// ---------------------------------------------------------------------------
// Schedule validation for create/update
// ---------------------------------------------------------------------------

pub fn validate_schedule(schedule: &CronSchedule) -> Result<(), CronError> {
    match schedule {
        CronSchedule::At { .. } => Ok(()),
        CronSchedule::Every { every_ms, .. } => {
            if *every_ms <= 0 {
                return Err(CronError::InvalidSchedule("every_ms must be positive".into()));
            }
            Ok(())
        }
        CronSchedule::Cron { expr, tz, .. } => {
            if expr.trim().is_empty() {
                return Ok(());
            }
            validate_cron_expression(expr)?;
            if let Some(tz_str) = tz {
                validate_timezone(tz_str)?;
            }
            Ok(())
        }
    }
}

// ---------------------------------------------------------------------------
// CronScheduler — manages tokio timers for scheduled jobs
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScheduledTick {
    pub job_id: String,
    pub scheduled_at: TimestampMs,
}

pub type TickCallback = Arc<dyn Fn(ScheduledTick) + Send + Sync>;

pub struct CronScheduler {
    handles: DashMap<String, JoinHandle<()>>,
    tick_callback: TickCallback,
}

impl CronScheduler {
    pub fn new(tick_callback: TickCallback) -> Self {
        Self {
            handles: DashMap::new(),
            tick_callback,
        }
    }

    pub fn schedule_job(&self, job: &CronJob) {
        self.cancel_job(&job.id);

        if !job.enabled {
            return;
        }

        let Some(next_run_at) = job.next_run_at else {
            return;
        };

        let job_id = job.id.clone();
        let schedule = job.schedule.clone();
        let callback = Arc::clone(&self.tick_callback);

        let handle = match &schedule {
            CronSchedule::At { .. } => spawn_at_timer(job_id, next_run_at, callback),
            CronSchedule::Every { every_ms, .. } => spawn_every_timer(job_id, next_run_at, *every_ms, callback),
            CronSchedule::Cron { expr, tz, .. } => {
                spawn_cron_timer(job_id, next_run_at, expr.clone(), tz.clone(), callback)
            }
        };

        self.handles.insert(job.id.clone(), handle);
    }

    pub fn cancel_job(&self, job_id: &str) {
        if let Some((_, handle)) = self.handles.remove(job_id) {
            handle.abort();
        }
    }

    pub fn reschedule_job(&self, job: &CronJob) {
        self.schedule_job(job);
    }

    pub fn schedule_retry(&self, job_id: &str, scheduled_at: TimestampMs, retry_at: TimestampMs) {
        self.cancel_job(job_id);
        let handle = spawn_occurrence_timer(
            job_id.to_owned(),
            scheduled_at,
            retry_at,
            Arc::clone(&self.tick_callback),
        );
        self.handles.insert(job_id.to_owned(), handle);
    }

    pub fn cancel_all(&self) {
        for entry in self.handles.iter() {
            entry.value().abort();
        }
        self.handles.clear();
    }

    pub fn active_count(&self) -> usize {
        self.handles.len()
    }

    pub fn is_scheduled(&self, job_id: &str) -> bool {
        self.handles.contains_key(job_id)
    }
}

impl Drop for CronScheduler {
    fn drop(&mut self) {
        self.cancel_all();
    }
}

// ---------------------------------------------------------------------------
// Timer spawn helpers
// ---------------------------------------------------------------------------

fn spawn_at_timer(job_id: String, run_at: TimestampMs, callback: TickCallback) -> JoinHandle<()> {
    spawn_occurrence_timer(job_id, run_at, run_at, callback)
}

fn spawn_occurrence_timer(
    job_id: String,
    scheduled_at: TimestampMs,
    run_at: TimestampMs,
    callback: TickCallback,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let delay = delay_until(run_at);
        if delay > 0 {
            tokio::time::sleep(tokio::time::Duration::from_millis(delay as u64)).await;
        }
        callback(ScheduledTick { job_id, scheduled_at });
    })
}

fn spawn_every_timer(
    job_id: String,
    first_run_at: TimestampMs,
    every_ms: i64,
    callback: TickCallback,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let initial_delay = delay_until(first_run_at);
        if initial_delay > 0 {
            tokio::time::sleep(tokio::time::Duration::from_millis(initial_delay as u64)).await;
        }
        let mut scheduled_at = first_run_at;
        callback(ScheduledTick {
            job_id: job_id.clone(),
            scheduled_at,
        });

        let interval_duration = tokio::time::Duration::from_millis(every_ms as u64);
        let mut interval = tokio::time::interval(interval_duration);
        interval.tick().await; // first tick fires immediately, skip it
        loop {
            interval.tick().await;
            scheduled_at += every_ms;
            callback(ScheduledTick {
                job_id: job_id.clone(),
                scheduled_at,
            });
        }
    })
}

fn spawn_cron_timer(
    job_id: String,
    first_run_at: TimestampMs,
    expr: String,
    tz: Option<String>,
    callback: TickCallback,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let initial_delay = delay_until(first_run_at);
        if initial_delay > 0 {
            tokio::time::sleep(tokio::time::Duration::from_millis(initial_delay as u64)).await;
        }
        callback(ScheduledTick {
            job_id: job_id.clone(),
            scheduled_at: first_run_at,
        });

        loop {
            let now = now_ms();
            let next = compute_cron_next_run(&expr, tz.as_deref(), now);
            let Some(next_at) = next else {
                break;
            };
            let delay = delay_until(next_at);
            if delay > 0 {
                tokio::time::sleep(tokio::time::Duration::from_millis(delay as u64)).await;
            }
            callback(ScheduledTick {
                job_id: job_id.clone(),
                scheduled_at: next_at,
            });
        }
    })
}

fn delay_until(target_ms: TimestampMs) -> i64 {
    let now = now_ms();
    (target_ms - now).max(0)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -- missed_run ----------------------------------------------------------

    const MIN: i64 = 60_000;
    const HOUR: i64 = 60 * MIN;

    fn every(every_ms: i64) -> CronSchedule {
        CronSchedule::Every {
            every_ms,
            description: None,
        }
    }

    fn cron(expr: &str, tz: &str) -> CronSchedule {
        CronSchedule::Cron {
            expr: expr.into(),
            tz: Some(tz.into()),
            description: None,
        }
    }

    fn utc(rfc3339: &str) -> TimestampMs {
        chrono::DateTime::parse_from_rfc3339(rfc3339)
            .unwrap()
            .timestamp_millis()
    }

    #[test]
    fn a_run_late_by_less_than_half_its_interval_is_caught_up_at_its_latest_occurrence() {
        let due = 1_000_000_000_000;
        // Slept through four half-hourly runs; the latest was ten minutes ago.
        let now = due + 3 * 30 * MIN + 10 * MIN;
        assert_eq!(
            missed_run(&every(30 * MIN), due, now),
            MissedRun::CatchUp {
                scheduled_at: due + 3 * 30 * MIN
            }
        );
    }

    #[test]
    fn a_frequent_run_too_late_to_catch_up_passes_for_the_next() {
        let due = 1_000_000_000_000;
        assert_eq!(missed_run(&every(30 * MIN), due, due + 20 * MIN), MissedRun::Pass);
    }

    #[test]
    fn a_daily_run_is_caught_up_within_two_hours_and_missed_after() {
        let schedule = cron("0 0 9 * * *", "Asia/Shanghai");
        let due = utc("2026-11-25T01:00:00Z"); // 09:00 in Shanghai
        assert_eq!(
            missed_run(&schedule, due, due + 90 * MIN),
            MissedRun::CatchUp { scheduled_at: due }
        );
        assert_eq!(missed_run(&schedule, due, due + 5 * HOUR), MissedRun::Missed);
    }

    #[test]
    fn the_night_gap_of_a_patrol_is_not_reported_missed() {
        // Half-hourly from 07:00 to 01:30 in New York; the machine slept from
        // 01:00 until 06:50 and woke ten minutes before the morning's first run.
        let schedule = cron("0 0,30 0,7-23 * * *", "America/New_York");
        let due = utc("2026-11-25T06:30:00Z"); // 01:30 in New York
        let now = utc("2026-11-25T11:50:00Z"); // 06:50 in New York
        assert_eq!(missed_run(&schedule, due, now), MissedRun::Pass);
    }

    #[test]
    fn a_one_time_run_is_caught_up_within_two_hours_and_missed_after() {
        let at = CronSchedule::At {
            at_ms: 1_000_000_000_000,
            description: None,
        };
        assert_eq!(
            missed_run(&at, 1_000_000_000_000, 1_000_000_000_000 + HOUR),
            MissedRun::CatchUp {
                scheduled_at: 1_000_000_000_000
            }
        );
        assert_eq!(
            missed_run(&at, 1_000_000_000_000, 1_000_000_000_000 + 3 * HOUR),
            MissedRun::Missed
        );
    }

    #[test]
    fn a_sleep_is_wall_time_the_timers_clock_did_not_count() {
        assert!(!slept_between(30_000, 30_000));
        assert!(!slept_between(80_000, 30_000), "a busy runtime may lag a little");
        assert!(slept_between(8 * HOUR, 30_000));
    }

    // -- compute_next_run ----------------------------------------------------

    #[test]
    fn next_run_at_returns_at_ms() {
        let schedule = CronSchedule::At {
            at_ms: 5000,
            description: None,
        };
        assert_eq!(compute_next_run(&schedule, 1000), Some(5000));
    }

    #[test]
    fn next_run_at_past_still_returns_at_ms() {
        let schedule = CronSchedule::At {
            at_ms: 500,
            description: None,
        };
        assert_eq!(compute_next_run(&schedule, 1000), Some(500));
    }

    #[test]
    fn next_run_every_adds_interval() {
        let schedule = CronSchedule::Every {
            every_ms: 60000,
            description: None,
        };
        assert_eq!(compute_next_run(&schedule, 1000), Some(61000));
    }

    #[test]
    fn next_run_every_zero_returns_none() {
        let schedule = CronSchedule::Every {
            every_ms: 0,
            description: None,
        };
        assert_eq!(compute_next_run(&schedule, 1000), None);
    }

    #[test]
    fn next_run_every_negative_returns_none() {
        let schedule = CronSchedule::Every {
            every_ms: -100,
            description: None,
        };
        assert_eq!(compute_next_run(&schedule, 1000), None);
    }

    #[test]
    fn next_run_after_occurrence_keeps_every_schedule_grid() {
        let schedule = CronSchedule::Every {
            every_ms: 1_000,
            description: None,
        };

        assert_eq!(
            compute_next_run_after_occurrence(&schedule, 10_000, 10_100),
            Some(11_000)
        );
        assert_eq!(
            compute_next_run_after_occurrence(&schedule, 10_000, 12_500),
            Some(13_000)
        );
    }

    #[test]
    fn next_run_cron_returns_future_time() {
        let now = now_ms();
        let schedule = CronSchedule::Cron {
            expr: "0 * * * * *".into(), // every minute
            tz: None,
            description: None,
        };
        let next = compute_next_run(&schedule, now);
        assert!(next.is_some());
        assert!(next.unwrap() > now);
    }

    #[test]
    fn next_run_cron_with_timezone() {
        let now = now_ms();
        let schedule = CronSchedule::Cron {
            expr: "0 * * * * *".into(),
            tz: Some("Asia/Shanghai".into()),
            description: None,
        };
        let next = compute_next_run(&schedule, now);
        assert!(next.is_some());
        assert!(next.unwrap() > now);
    }

    #[test]
    fn next_run_cron_invalid_expr_returns_none() {
        let schedule = CronSchedule::Cron {
            expr: "invalid".into(),
            tz: None,
            description: None,
        };
        assert_eq!(compute_next_run(&schedule, 1000), None);
    }

    #[test]
    fn next_run_cron_invalid_tz_returns_none() {
        let schedule = CronSchedule::Cron {
            expr: "0 * * * * *".into(),
            tz: Some("Mars/Olympus".into()),
            description: None,
        };
        assert_eq!(compute_next_run(&schedule, 1000), None);
    }

    // -- validate_schedule ---------------------------------------------------

    #[test]
    fn validate_at_schedule() {
        let s = CronSchedule::At {
            at_ms: 1000,
            description: None,
        };
        assert!(validate_schedule(&s).is_ok());
    }

    #[test]
    fn validate_every_positive() {
        let s = CronSchedule::Every {
            every_ms: 1000,
            description: None,
        };
        assert!(validate_schedule(&s).is_ok());
    }

    #[test]
    fn validate_every_zero_fails() {
        let s = CronSchedule::Every {
            every_ms: 0,
            description: None,
        };
        assert!(validate_schedule(&s).is_err());
    }

    #[test]
    fn validate_every_negative_fails() {
        let s = CronSchedule::Every {
            every_ms: -1,
            description: None,
        };
        assert!(validate_schedule(&s).is_err());
    }

    #[test]
    fn validate_cron_valid() {
        let s = CronSchedule::Cron {
            expr: "0 */5 * * * *".into(),
            tz: None,
            description: None,
        };
        assert!(validate_schedule(&s).is_ok());
    }

    #[test]
    fn validate_cron_empty_expr_is_manual_only() {
        let s = CronSchedule::Cron {
            expr: String::new(),
            tz: None,
            description: Some("manual".into()),
        };
        assert!(validate_schedule(&s).is_ok());
        assert_eq!(compute_next_run(&s, 1000), None);
    }

    #[test]
    fn validate_cron_with_valid_tz() {
        let s = CronSchedule::Cron {
            expr: "0 0 9 * * *".into(),
            tz: Some("Asia/Shanghai".into()),
            description: None,
        };
        assert!(validate_schedule(&s).is_ok());
    }

    #[test]
    fn validate_cron_invalid_expr() {
        let s = CronSchedule::Cron {
            expr: "invalid".into(),
            tz: None,
            description: None,
        };
        let err = validate_schedule(&s).unwrap_err();
        assert!(matches!(err, CronError::InvalidCronExpression(_)));
    }

    #[test]
    fn validate_cron_invalid_tz() {
        let s = CronSchedule::Cron {
            expr: "0 * * * * *".into(),
            tz: Some("Invalid/TZ".into()),
            description: None,
        };
        let err = validate_schedule(&s).unwrap_err();
        assert!(matches!(err, CronError::InvalidTimezone(_)));
    }

    // -- validate_cron_expression / validate_timezone -------------------------

    #[test]
    fn validate_cron_expression_valid() {
        assert!(validate_cron_expression("0 */5 * * * *").is_ok());
        assert!(validate_cron_expression("0 0 9 * * *").is_ok());
        assert!(validate_cron_expression("0 0 0 1 1 *").is_ok());
    }

    #[test]
    fn validate_cron_expression_accepts_five_field_unix_form() {
        // Standard 5-field Unix cron (minute hour day month dow) — must be
        // auto-normalized to the 6-field form the `cron` crate requires.
        assert!(validate_cron_expression("0 9 * * *").is_ok());
        assert!(validate_cron_expression("30 14 * * MON-FRI").is_ok());
        assert!(validate_cron_expression("0 10 * * WED").is_ok());
        assert!(validate_cron_expression("0 * * * *").is_ok());
    }

    #[test]
    fn validate_cron_expression_invalid() {
        assert!(validate_cron_expression("not a cron").is_err());
        assert!(validate_cron_expression("").is_err());
    }

    #[test]
    fn normalize_cron_expr_leaves_six_field_alone() {
        assert_eq!(normalize_cron_expr("0 0 9 * * *"), "0 0 9 * * *");
    }

    #[test]
    fn normalize_cron_expr_promotes_five_field() {
        assert_eq!(normalize_cron_expr("0 9 * * *"), "0 0 9 * * *");
        assert_eq!(normalize_cron_expr("  30 14 * * MON-FRI  "), "0 30 14 * * MON-FRI");
    }

    #[test]
    fn validate_timezone_valid() {
        assert!(validate_timezone("UTC").is_ok());
        assert!(validate_timezone("Asia/Shanghai").is_ok());
        assert!(validate_timezone("America/New_York").is_ok());
    }

    #[test]
    fn validate_timezone_invalid() {
        assert!(validate_timezone("Invalid/TZ").is_err());
        assert!(validate_timezone("Mars").is_err());
    }

    // -- CronScheduler -------------------------------------------------------

    #[tokio::test]
    async fn scheduler_schedule_and_cancel() {
        let called = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let called_clone = Arc::clone(&called);
        let scheduler = CronScheduler::new(Arc::new(move |_id| {
            called_clone.store(true, std::sync::atomic::Ordering::SeqCst);
        }));

        let job = make_test_job("cron_1", true, Some(now_ms() + 100_000));
        scheduler.schedule_job(&job);
        assert!(scheduler.is_scheduled("cron_1"));
        assert_eq!(scheduler.active_count(), 1);

        scheduler.cancel_job("cron_1");
        assert!(!scheduler.is_scheduled("cron_1"));
        assert_eq!(scheduler.active_count(), 0);
    }

    #[tokio::test]
    async fn scheduler_disabled_job_not_scheduled() {
        let scheduler = CronScheduler::new(Arc::new(|_| {}));
        let job = make_test_job("cron_1", false, Some(now_ms() + 100_000));
        scheduler.schedule_job(&job);
        assert!(!scheduler.is_scheduled("cron_1"));
    }

    #[tokio::test]
    async fn scheduler_no_next_run_not_scheduled() {
        let scheduler = CronScheduler::new(Arc::new(|_| {}));
        let job = make_test_job("cron_1", true, None);
        scheduler.schedule_job(&job);
        assert!(!scheduler.is_scheduled("cron_1"));
    }

    #[tokio::test]
    async fn scheduler_cancel_all() {
        let scheduler = CronScheduler::new(Arc::new(|_| {}));
        let future = now_ms() + 100_000;
        scheduler.schedule_job(&make_test_job("cron_1", true, Some(future)));
        scheduler.schedule_job(&make_test_job("cron_2", true, Some(future)));
        scheduler.schedule_job(&make_test_job("cron_3", true, Some(future)));
        assert_eq!(scheduler.active_count(), 3);

        scheduler.cancel_all();
        assert_eq!(scheduler.active_count(), 0);
    }

    #[tokio::test]
    async fn scheduler_reschedule_replaces_timer() {
        let scheduler = CronScheduler::new(Arc::new(|_| {}));
        let job = make_test_job("cron_1", true, Some(now_ms() + 100_000));
        scheduler.schedule_job(&job);
        assert!(scheduler.is_scheduled("cron_1"));

        let updated = CronJob {
            next_run_at: Some(now_ms() + 200_000),
            ..job
        };
        scheduler.reschedule_job(&updated);
        assert!(scheduler.is_scheduled("cron_1"));
        assert_eq!(scheduler.active_count(), 1);
    }

    #[tokio::test]
    async fn scheduler_cancel_nonexistent_no_panic() {
        let scheduler = CronScheduler::new(Arc::new(|_| {}));
        scheduler.cancel_job("nonexistent");
    }

    #[tokio::test]
    async fn scheduler_at_timer_fires_callback() {
        let (tx, rx) = tokio::sync::oneshot::channel::<ScheduledTick>();
        let tx = Arc::new(std::sync::Mutex::new(Some(tx)));
        let scheduler = CronScheduler::new(Arc::new(move |id| {
            if let Some(sender) = tx.lock().unwrap().take() {
                let _ = sender.send(id);
            }
        }));

        let job = CronJob {
            schedule: CronSchedule::At {
                at_ms: now_ms() + 50,
                description: None,
            },
            next_run_at: Some(now_ms() + 50),
            ..make_test_job("cron_at", true, Some(now_ms() + 50))
        };
        scheduler.schedule_job(&job);

        let result = tokio::time::timeout(tokio::time::Duration::from_secs(2), rx).await;
        assert!(result.is_ok());
        let tick = result.unwrap().unwrap();
        assert_eq!(tick.job_id, "cron_at");
        assert_eq!(tick.scheduled_at, job.next_run_at.unwrap());
    }

    #[tokio::test]
    async fn scheduler_every_timer_fires_callback() {
        let counter = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let counter_clone = Arc::clone(&counter);
        let scheduler = CronScheduler::new(Arc::new(move |_id| {
            counter_clone.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }));

        let job = CronJob {
            schedule: CronSchedule::Every {
                every_ms: 50,
                description: None,
            },
            next_run_at: Some(now_ms() + 50),
            ..make_test_job("cron_every", true, Some(now_ms() + 50))
        };
        scheduler.schedule_job(&job);

        tokio::time::sleep(tokio::time::Duration::from_millis(250)).await;
        scheduler.cancel_job("cron_every");

        let count = counter.load(std::sync::atomic::Ordering::SeqCst);
        assert!(count >= 2, "expected at least 2 ticks, got {count}");
    }

    // -- Test helper ----------------------------------------------------------

    fn make_test_job(id: &str, enabled: bool, next_run_at: Option<TimestampMs>) -> CronJob {
        use crate::types::{CreatedBy, ExecutionMode};
        CronJob {
            id: id.to_owned(),
            user_id: "user1".into(),
            name: "Test".into(),
            enabled,
            schedule: CronSchedule::Every {
                every_ms: 60000,
                description: None,
            },
            message: "test message".into(),
            execution_mode: ExecutionMode::Existing,
            agent_config: None,
            conversation_id: "conv_1".into(),
            conversation_title: None,
            agent_type: "acp".into(),
            created_by: CreatedBy::User,
            skill_content: None,
            description: None,
            created_at: 1000,
            updated_at: 1000,
            next_run_at,
            last_run_at: None,
            last_status: None,
            last_error: None,
            run_count: 0,
            retry_count: 0,
            max_retries: 3,
            queue_enabled: false,
        }
    }
}
