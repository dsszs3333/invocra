use std::num::NonZeroU32;
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, UNIX_EPOCH};

use reqwest::{header::HeaderMap, StatusCode};
use tokio::sync::{Mutex as AsyncMutex, Notify};
use tokio::time::Instant;

const MAX_SERVER_WAIT: Duration = Duration::from_secs(24 * 60 * 60);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Priority {
    Interactive = 0,
    OpenPipeline = 1,
    Background = 2,
}

/// Snapshot of this client's shared request budget, for a non-blocking UI.
#[derive(Debug, Clone, Default)]
pub struct RequestStatus {
    pub limit: Option<NonZeroU32>,
    pub waiting: usize,
    pub wait: Duration,
    pub server_pause: Duration,
    pub rate_limited: bool,
}

#[derive(Debug)]
struct State {
    limit: Option<NonZeroU32>,
    next: Instant,
    last_sent: Instant,
    pause: Instant,
    quota: Option<Quota>,
    in_flight: u64,
    waiting: [usize; 3],
    open_streak: u8,
    backoff: Duration,
    rate_limited: bool,
}

#[derive(Debug)]
struct Quota {
    reset: u64,
    until: Instant,
    remaining: u64,
    reserve: u64,
}

impl State {
    fn server_deadline(&self, priority: Priority, now: Instant) -> Instant {
        let Some(quota) = self.quota.as_ref().filter(|quota| quota.until > now) else {
            return self.pause;
        };
        let quota_deadline = if quota.remaining == 0
            || (priority != Priority::Interactive && quota.remaining <= quota.reserve)
        {
            quota.until
        } else if priority == Priority::Interactive {
            now
        } else {
            let slots = u32::try_from(quota.remaining - quota.reserve).unwrap_or(u32::MAX);
            let interval = quota.until.saturating_duration_since(self.last_sent) / slots;
            deadline(
                self.last_sent,
                interval.saturating_add(Duration::from_nanos(1)),
            )
        };
        self.pause.max(quota_deadline)
    }
}

#[derive(Debug)]
pub(crate) struct RateLimiter {
    state: Mutex<State>,
    changed: Notify,
    turns: [AsyncMutex<()>; 3],
}

impl Default for RateLimiter {
    fn default() -> Self {
        Self {
            state: Mutex::new(State {
                limit: None,
                next: Instant::now(),
                last_sent: Instant::now(),
                pause: Instant::now(),
                quota: None,
                in_flight: 0,
                waiting: [0; 3],
                open_streak: 0,
                backoff: Duration::from_secs(1),
                rate_limited: false,
            }),
            changed: Notify::new(),
            turns: std::array::from_fn(|_| AsyncMutex::new(())),
        }
    }
}

impl RateLimiter {
    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|err| err.into_inner())
    }

    pub(crate) fn configure(&self, limit: Option<NonZeroU32>) {
        self.state().limit = limit;
        self.changed.notify_waiters();
    }

    pub(crate) fn status(&self) -> RequestStatus {
        let state = self.state();
        let now = Instant::now();
        RequestStatus {
            limit: state.limit,
            waiting: state.waiting.iter().sum(),
            wait: state
                .next
                .max(state.server_deadline(
                    if state.waiting[0] > 0 {
                        Priority::Interactive
                    } else {
                        Priority::Background
                    },
                    now,
                ))
                .saturating_duration_since(now),
            server_pause: state
                .server_deadline(Priority::Interactive, now)
                .saturating_duration_since(now),
            rate_limited: state.rate_limited,
        }
    }

    pub(crate) async fn acquire(&self, priority: Priority) -> Request<'_> {
        let _waiting = Waiting::new(self, priority);
        // FIFO within each class; two open-pipeline slots then one background slot under load.
        let _turn = self.turns[priority as usize].lock().await;
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            let deadline = {
                let mut state = self.state();
                let now = Instant::now();
                let deadline = state.next.max(state.server_deadline(priority, now));
                let blocked = match priority {
                    Priority::Interactive => false,
                    Priority::OpenPipeline => {
                        state.waiting[0] > 0 || (state.waiting[2] > 0 && state.open_streak >= 2)
                    }
                    Priority::Background => {
                        state.waiting[0] > 0 || (state.waiting[1] > 0 && state.open_streak < 2)
                    }
                };
                if blocked {
                    None
                } else if deadline > now {
                    Some(deadline)
                } else {
                    let interval = state.limit.map_or(Duration::ZERO, |limit| {
                        Duration::from_nanos(60_000_000_000u64.div_ceil(u64::from(limit.get())))
                    });
                    match priority {
                        Priority::OpenPipeline => {
                            state.open_streak = (state.open_streak + 1).min(2)
                        }
                        Priority::Background => state.open_streak = 0,
                        Priority::Interactive => {}
                    }
                    state.next = now + interval;
                    state.last_sent = now;
                    state.in_flight += 1;
                    if let Some(quota) = state.quota.as_mut().filter(|quota| quota.until > now) {
                        quota.remaining = quota.remaining.saturating_sub(1);
                    }
                    return Request { limiter: self };
                }
            };
            match deadline {
                Some(deadline) => tokio::select! {
                    () = tokio::time::sleep_until(deadline) => {},
                    () = &mut changed => {},
                },
                None => changed.await,
            }
        }
    }

    fn observe(&self, status: StatusCode, headers: &HeaderMap) {
        let mut state = self.state();
        let now = Instant::now();
        if status == StatusCode::TOO_MANY_REQUESTS {
            state.rate_limited = true;
            let wait = crate::retry_after(headers)
                .unwrap_or(state.backoff)
                .min(MAX_SERVER_WAIT);
            // Bound malformed server deadlines while preserving ordinary Retry-After windows.
            state.pause = state.pause.max(deadline(now, wait));
            state.backoff = (state.backoff * 2).min(Duration::from_secs(60));
        } else if status.is_success() && now >= state.pause {
            state.backoff = Duration::from_secs(1);
            state.rate_limited = false;
        }
        if let Some(mut quota) = server_quota(headers, now) {
            // Other in-flight requests may not be reflected in this response's counter yet.
            quota.remaining = quota
                .remaining
                .saturating_sub(state.in_flight.saturating_sub(1));
            match state.quota.as_mut() {
                Some(current) if current.reset == quota.reset => {
                    current.remaining = current.remaining.min(quota.remaining);
                }
                Some(current) if current.reset > quota.reset => {}
                _ => state.quota = Some(quota),
            }
        }
        drop(state);
        self.changed.notify_waiters();
    }
}

fn server_quota(headers: &HeaderMap, now: Instant) -> Option<Quota> {
    let remaining = header_number(headers, "ratelimit-remaining")?;
    let reset = header_number(headers, "ratelimit-reset")?;
    let epoch = crate::server_time(headers)
        .duration_since(UNIX_EPOCH)
        .ok()?;
    let wait = Duration::from_secs(reset).checked_sub(epoch)?;
    if wait > MAX_SERVER_WAIT {
        return None;
    }
    Some(Quota {
        reset,
        until: now.checked_add(wait)?,
        remaining,
        reserve: remaining.div_ceil(10).max(1),
    })
}

pub(crate) struct Request<'a> {
    limiter: &'a RateLimiter,
}

impl Request<'_> {
    pub(crate) fn observe(self, status: StatusCode, headers: &HeaderMap) {
        self.limiter.observe(status, headers);
    }
}

impl Drop for Request<'_> {
    fn drop(&mut self) {
        self.limiter.state().in_flight -= 1;
    }
}

fn header_number(headers: &HeaderMap, name: &str) -> Option<u64> {
    headers.get(name)?.to_str().ok()?.trim().parse().ok()
}

fn deadline(now: Instant, wait: Duration) -> Instant {
    // An unrepresentable server deadline must not panic or permit an early retry.
    now.checked_add(wait)
        .unwrap_or_else(|| now + Duration::from_secs(u32::MAX.into()))
}

struct Waiting<'a> {
    limiter: &'a RateLimiter,
    priority: Priority,
}

impl<'a> Waiting<'a> {
    fn new(limiter: &'a RateLimiter, priority: Priority) -> Self {
        let mut state = limiter.state();
        state.waiting[priority as usize] += 1;
        Self { limiter, priority }
    }
}

impl Drop for Waiting<'_> {
    fn drop(&mut self) {
        let mut state = self.limiter.state();
        state.waiting[self.priority as usize] -= 1;
        drop(state);
        self.limiter.changed.notify_waiters();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::time::SystemTime;

    fn limited() -> Arc<RateLimiter> {
        let limiter = Arc::new(RateLimiter::default());
        limiter.configure(NonZeroU32::new(60));
        limiter
    }

    fn quota_headers(remaining: u64, reset: u64) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            "ratelimit-remaining",
            remaining.to_string().parse().expect("remaining"),
        );
        headers.insert("ratelimit-reset", reset.to_string().parse().expect("reset"));
        headers
    }

    fn reset_after(seconds: u64) -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("epoch")
            .as_secs()
            + seconds
    }

    #[tokio::test(start_paused = true)]
    async fn background_slows_as_other_clients_consume_quota() {
        let limiter = RateLimiter::default();
        let reset = reset_after(60);
        limiter.observe(StatusCode::OK, &quota_headers(21, reset));
        let start = Instant::now();
        limiter.acquire(Priority::OpenPipeline).await;
        assert!(Instant::now() - start >= Duration::from_secs(3));
        limiter.observe(StatusCode::OK, &quota_headers(9, reset));
        let wait = limiter.status().wait;
        assert!(wait >= Duration::from_secs(9));
        limiter.observe(StatusCode::OK, &quota_headers(100, reset));
        assert_eq!(limiter.status().wait, wait);
        let before = Instant::now();
        limiter.acquire(Priority::Background).await;
        assert!(Instant::now() - before >= wait);
        assert!(!limiter.status().rate_limited);
    }

    #[tokio::test(start_paused = true)]
    async fn automatic_polling_leaves_a_reserve_that_only_manual_requests_can_use() {
        let limiter = Arc::new(RateLimiter::default());
        limiter.observe(StatusCode::OK, &quota_headers(3, reset_after(60)));
        let start = Instant::now();
        limiter.acquire(Priority::Background).await;
        assert!(Instant::now() - start >= Duration::from_secs(29));
        limiter.acquire(Priority::Interactive).await;
        let automatic = limiter.clone();
        let task = tokio::spawn(async move {
            automatic.acquire(Priority::OpenPipeline).await;
            Instant::now()
        });
        tokio::task::yield_now().await;
        assert!(!task.is_finished());
        assert!(limiter.status().wait >= Duration::from_secs(29));
        assert_eq!(limiter.status().server_pause, Duration::ZERO);
        let before = Instant::now();
        limiter.acquire(Priority::Interactive).await;
        assert_eq!(Instant::now(), before);
        assert!(limiter.status().server_pause >= Duration::from_secs(29));
        RateLimiter::default().acquire(Priority::Background).await;
        assert_eq!(Instant::now(), before);
        assert!(task.await.expect("poll") - start >= Duration::from_secs(59));
        let after_reset = Instant::now();
        limiter.acquire(Priority::Background).await;
        assert_eq!(Instant::now(), after_reset);
        assert_eq!(limiter.status().waiting, 0);
    }

    #[tokio::test(start_paused = true)]
    async fn server_quota_and_manual_priority_never_relax_local_budgets() {
        for budget in [20, 80, 100] {
            let limiter = Arc::new(RateLimiter::default());
            limiter.configure(NonZeroU32::new(budget));
            limiter.observe(StatusCode::OK, &quota_headers(1_000_000, reset_after(60)));
            let interval = Duration::from_nanos(60_000_000_000u64.div_ceil(u64::from(budget)));
            limiter.acquire(Priority::Background).await;
            for priority in [
                Priority::Interactive,
                Priority::OpenPipeline,
                Priority::Background,
            ] {
                let before = Instant::now();
                limiter.acquire(priority).await;
                assert!(Instant::now() - before >= interval);
            }
        }
    }

    #[tokio::test(start_paused = true)]
    async fn parallel_responses_cannot_replenish_spent_or_in_flight_quota() {
        let limiter = RateLimiter::default();
        let reset = reset_after(60);
        let first = limiter.acquire(Priority::Interactive).await;
        let second = limiter.acquire(Priority::Interactive).await;
        second.observe(StatusCode::OK, &quota_headers(10, reset));
        assert_eq!(limiter.state().quota.as_ref().expect("quota").remaining, 9);
        first.observe(StatusCode::OK, &quota_headers(11, reset));
        assert_eq!(limiter.state().quota.as_ref().expect("quota").remaining, 9);
        let request = limiter.acquire(Priority::Interactive).await;
        request.observe(StatusCode::OK, &quota_headers(100, reset));
        assert_eq!(limiter.state().quota.as_ref().expect("quota").remaining, 8);
        assert_eq!(limiter.state().in_flight, 0);
    }

    #[tokio::test(start_paused = true)]
    async fn new_quota_window_replenishes_without_replaying_old_responses_or_pauses() {
        let limiter = RateLimiter::default();
        let reset = reset_after(60);
        limiter.observe(StatusCode::OK, &quota_headers(0, reset));
        assert!(limiter.status().server_pause >= Duration::from_secs(59));
        limiter.observe(StatusCode::OK, &quota_headers(20, reset + 60));
        limiter.observe(StatusCode::OK, &quota_headers(0, reset));
        let before = Instant::now();
        limiter.acquire(Priority::Interactive).await;
        assert_eq!(Instant::now(), before);
        assert_eq!(limiter.state().quota.as_ref().expect("quota").remaining, 19);
        let until = limiter.state().quota.as_ref().expect("quota").until;
        tokio::time::advance(until - Instant::now()).await;
        limiter.observe(StatusCode::OK, &quota_headers(0, reset + 60));
        let before = Instant::now();
        limiter.acquire(Priority::Background).await;
        assert_eq!(Instant::now(), before);
        assert_eq!(limiter.status().server_pause, Duration::ZERO);
    }

    #[tokio::test(start_paused = true)]
    async fn absent_invalid_and_expired_quota_headers_preserve_existing_pacing() {
        let reset = reset_after(60).to_string();
        for (remaining, reset_value) in [
            (None, None),
            (Some("10"), None),
            (None, Some(reset.as_str())),
            (Some("-1"), Some(reset.as_str())),
            (Some("1.5"), Some(reset.as_str())),
            (Some("invalid"), Some(reset.as_str())),
            (Some("10"), Some("invalid")),
            (Some("10"), Some("0")),
            (Some("10"), Some("18446744073709551615")),
        ] {
            let mut headers = HeaderMap::new();
            if let Some(remaining) = remaining {
                headers.insert("ratelimit-remaining", remaining.parse().expect("remaining"));
            }
            if let Some(reset) = reset_value {
                headers.insert("ratelimit-reset", reset.parse().expect("reset"));
            }
            let limiter = RateLimiter::default();
            limiter.observe(StatusCode::OK, &headers);
            assert!(limiter.state().quota.is_none());
            limiter.observe(StatusCode::OK, &quota_headers(10, reset_after(60)));
            let wait = limiter.status().wait;
            limiter.observe(StatusCode::OK, &headers);
            assert_eq!(limiter.status().wait, wait);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn positive_remaining_quota_does_not_bypass_429_or_shorten_retry_after() {
        let limiter = RateLimiter::default();
        let headers = quota_headers(100, reset_after(60));
        let mut throttled = headers.clone();
        throttled.insert("retry-after", "120".parse().expect("retry-after"));
        limiter.observe(StatusCode::TOO_MANY_REQUESTS, &throttled);
        limiter.observe(StatusCode::OK, &headers);
        let before = Instant::now();
        limiter.acquire(Priority::Interactive).await;
        assert_eq!(Instant::now() - before, Duration::from_secs(120));
        assert!(limiter.status().rate_limited);
    }

    #[tokio::test(start_paused = true)]
    async fn successful_request_after_cooldown_clears_the_429_hint() {
        let limiter = RateLimiter::default();
        limiter.observe(StatusCode::TOO_MANY_REQUESTS, &HeaderMap::new());
        limiter.observe(StatusCode::OK, &HeaderMap::new());
        assert!(limiter.status().rate_limited);
        let request = limiter.acquire(Priority::Interactive).await;
        request.observe(StatusCode::OK, &HeaderMap::new());
        assert!(!limiter.status().rate_limited);
        assert_eq!(limiter.status().server_pause, Duration::ZERO);
    }

    #[tokio::test(start_paused = true)]
    async fn quota_uses_server_clock_and_bounds_unreasonable_reset_times() {
        let limiter = RateLimiter::default();
        for skew in [-3600_i64, 3600] {
            let server_epoch = reset_after(0)
                .checked_add_signed(skew)
                .expect("server epoch");
            let mut headers = quota_headers(0, server_epoch + 60);
            headers.insert(
                "date",
                httpdate::fmt_http_date(UNIX_EPOCH + Duration::from_secs(server_epoch))
                    .parse()
                    .expect("date"),
            );
            let quota = server_quota(&headers, Instant::now()).expect("quota");
            assert_eq!(quota.until - Instant::now(), Duration::from_secs(60));
        }
        limiter.observe(StatusCode::OK, &quota_headers(0, u64::MAX));
        assert!(limiter.state().quota.is_none());
        limiter.observe(StatusCode::OK, &quota_headers(10, reset_after(60)));
        assert!(limiter.state().quota.is_some());
    }

    #[tokio::test(start_paused = true)]
    async fn cancelling_a_sent_request_releases_in_flight_count_without_refunding_quota() {
        let limiter = Arc::new(RateLimiter::default());
        limiter.observe(StatusCode::OK, &quota_headers(10, reset_after(60)));
        let sending = limiter.clone();
        let task = tokio::spawn(async move {
            let _request = sending.acquire(Priority::Interactive).await;
            std::future::pending::<()>().await;
        });
        tokio::task::yield_now().await;
        assert_eq!(limiter.state().in_flight, 1);
        task.abort();
        assert!(task.await.expect_err("cancelled").is_cancelled());
        assert_eq!(limiter.state().in_flight, 0);
        assert_eq!(limiter.state().quota.as_ref().expect("quota").remaining, 9);
    }

    #[tokio::test(start_paused = true)]
    async fn concurrent_requests_share_spacing_without_bursts() {
        let limiter = limited();
        let start = Instant::now();
        let mut tasks = tokio::task::JoinSet::new();
        for _ in 0..65 {
            let limiter = limiter.clone();
            tasks.spawn(async move {
                limiter.acquire(Priority::Interactive).await;
                Instant::now()
            });
        }
        let mut sent = Vec::new();
        while let Some(result) = tasks.join_next().await {
            sent.push(result.expect("request"));
        }
        sent.sort();
        assert_eq!(sent[0], start);
        assert!(sent
            .windows(2)
            .all(|pair| pair[1] - pair[0] >= Duration::from_secs(1)));
        assert!(sent[60] - sent[0] >= Duration::from_secs(60));
        assert_eq!(limiter.status().waiting, 0);
    }

    #[tokio::test(start_paused = true)]
    async fn foreground_overtakes_background_and_cancelled_waiter_releases_priority() {
        let limiter = limited();
        limiter.acquire(Priority::Interactive).await;
        let bg = limiter.clone();
        let background = tokio::spawn(async move {
            bg.acquire(Priority::Background).await;
            Instant::now()
        });
        tokio::task::yield_now().await;
        let fg = limiter.clone();
        let foreground = tokio::spawn(async move {
            fg.acquire(Priority::Interactive).await;
            Instant::now()
        });
        tokio::task::yield_now().await;
        let foreground_time = foreground.await.expect("foreground");
        let background_time = background.await.expect("background");
        assert!(foreground_time < background_time);

        let fg = limiter.clone();
        let cancelled = tokio::spawn(async move {
            fg.acquire(Priority::Interactive).await;
        });
        tokio::task::yield_now().await;
        cancelled.abort();
        assert!(cancelled.await.expect_err("cancelled").is_cancelled());
        assert_eq!(limiter.status().waiting, 0);
        let before = Instant::now();
        limiter.acquire(Priority::Background).await;
        assert_eq!(Instant::now() - before, Duration::from_secs(1));
    }

    #[tokio::test(start_paused = true)]
    async fn server_pause_is_shared_and_independent_of_local_limit() {
        let limiter = RateLimiter::default();
        let mut headers = HeaderMap::new();
        headers.insert("retry-after", "120".parse().expect("header"));
        limiter.observe(StatusCode::TOO_MANY_REQUESTS, &headers);
        limiter.configure(None);
        let other = RateLimiter::default();
        let before = Instant::now();
        other.acquire(Priority::Interactive).await;
        assert_eq!(Instant::now(), before);
        limiter.acquire(Priority::Interactive).await;
        assert!(Instant::now() - before >= Duration::from_secs(120));
        assert!(limiter.status().rate_limited);
    }

    #[tokio::test(start_paused = true)]
    async fn repeated_429_backs_off_and_success_does_not_shorten_existing_pause() {
        let limiter = RateLimiter::default();
        for seconds in [1, 2, 4] {
            limiter.observe(StatusCode::TOO_MANY_REQUESTS, &HeaderMap::new());
            limiter.observe(StatusCode::OK, &HeaderMap::new());
            let before = Instant::now();
            limiter.acquire(Priority::Interactive).await;
            assert_eq!(Instant::now() - before, Duration::from_secs(seconds));
        }
        limiter.observe(StatusCode::OK, &HeaderMap::new());
        limiter.observe(StatusCode::TOO_MANY_REQUESTS, &HeaderMap::new());
        assert_eq!(limiter.status().server_pause, Duration::from_secs(1));
    }

    #[tokio::test(start_paused = true)]
    async fn exhausted_server_quota_pauses_before_a_429() {
        let limiter = RateLimiter::default();
        let reset = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("epoch")
            .as_secs()
            + 30;
        let mut headers = HeaderMap::new();
        headers.insert("ratelimit-remaining", "0".parse().expect("remaining"));
        headers.insert("ratelimit-reset", reset.to_string().parse().expect("reset"));
        limiter.observe(StatusCode::OK, &headers);
        assert!(limiter.status().server_pause >= Duration::from_secs(29));
        assert!(!limiter.status().rate_limited);
        let before = Instant::now();
        limiter.acquire(Priority::Interactive).await;
        assert!(Instant::now() - before >= Duration::from_secs(29));
    }

    #[tokio::test(start_paused = true)]
    async fn invalid_headers_do_not_panic_or_override_a_server_pause() {
        let limiter = RateLimiter::default();
        let mut headers = HeaderMap::new();
        headers.insert("retry-after", "invalid".parse().expect("header"));
        headers.insert("ratelimit-reset", "invalid".parse().expect("header"));
        limiter.observe(StatusCode::TOO_MANY_REQUESTS, &headers);
        assert_eq!(limiter.status().server_pause, Duration::from_secs(1));
        headers.insert("retry-after", u64::MAX.to_string().parse().expect("header"));
        limiter.observe(StatusCode::TOO_MANY_REQUESTS, &headers);
        assert_eq!(limiter.status().server_pause, MAX_SERVER_WAIT);
    }
    #[tokio::test(start_paused = true)]
    async fn open_pipeline_gets_two_slots_then_background_at_all_supported_budgets() {
        for budget in [20, 80, 100] {
            let limiter = Arc::new(RateLimiter::default());
            limiter.configure(NonZeroU32::new(budget));
            limiter.observe(StatusCode::OK, &quota_headers(30, reset_after(60)));
            limiter.acquire(Priority::Interactive).await;
            let mut tasks = tokio::task::JoinSet::new();
            for priority in [Priority::OpenPipeline, Priority::Background] {
                for _ in 0..6 {
                    let limiter = limiter.clone();
                    tasks.spawn(async move {
                        limiter.acquire(priority).await;
                        (Instant::now(), priority)
                    });
                }
            }
            tokio::task::yield_now().await;
            let manual = limiter.clone();
            let user = tokio::spawn(async move {
                manual.acquire(Priority::Interactive).await;
                Instant::now()
            });
            let manual_time = user.await.expect("manual");
            let mut admitted = Vec::new();
            while let Some(result) = tasks.join_next().await {
                admitted.push(result.expect("poll"));
            }
            admitted.sort_by_key(|row| row.0);
            assert!(manual_time < admitted[0].0);
            let order: Vec<_> = admitted.iter().map(|row| row.1).collect();
            assert_eq!(
                &order[..9],
                &[
                    Priority::OpenPipeline,
                    Priority::OpenPipeline,
                    Priority::Background,
                    Priority::OpenPipeline,
                    Priority::OpenPipeline,
                    Priority::Background,
                    Priority::OpenPipeline,
                    Priority::OpenPipeline,
                    Priority::Background
                ]
            );
            let interval = Duration::from_nanos(60_000_000_000u64.div_ceil(u64::from(budget)));
            assert!(admitted
                .windows(2)
                .all(|pair| pair[1].0 - pair[0].0 >= interval));
            assert_eq!(limiter.status().waiting, 0);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn cancelling_open_pipeline_waiter_unblocks_background() {
        let limiter = limited();
        limiter.acquire(Priority::Interactive).await;
        let open = limiter.clone();
        let task = tokio::spawn(async move {
            open.acquire(Priority::OpenPipeline).await;
        });
        tokio::task::yield_now().await;
        task.abort();
        assert!(task.await.expect_err("cancelled").is_cancelled());
        limiter.acquire(Priority::Background).await;
        assert_eq!(limiter.status().waiting, 0);
    }
}
