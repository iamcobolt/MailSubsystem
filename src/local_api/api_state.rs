use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::db;

const DEFAULT_MAX_CONCURRENT_CHAT_STREAMS: usize = 8;
const DEFAULT_MAX_CHAT_REQUESTS_PER_WINDOW: usize = 30;
const DEFAULT_CHAT_REQUEST_WINDOW: Duration = Duration::from_secs(60);

#[derive(Clone)]
pub struct ApiState {
    pub db: Arc<db::Database>,
    pub account_id: String,
    chat_stream_limit: ChatStreamLimit,
    chat_request_rate_limiter: ChatRequestRateLimiter,
}

impl ApiState {
    pub fn new(db: Arc<db::Database>, account_id: impl Into<String>) -> Self {
        Self {
            db,
            account_id: account_id.into(),
            chat_stream_limit: ChatStreamLimit::new(DEFAULT_MAX_CONCURRENT_CHAT_STREAMS),
            chat_request_rate_limiter: ChatRequestRateLimiter::new(
                DEFAULT_MAX_CHAT_REQUESTS_PER_WINDOW,
                DEFAULT_CHAT_REQUEST_WINDOW,
            ),
        }
    }

    pub fn try_acquire_chat_stream(&self) -> Option<ChatStreamPermit> {
        self.chat_stream_limit.try_acquire()
    }

    pub fn allow_chat_request(&self) -> bool {
        self.chat_request_rate_limiter.allow()
    }
}

#[derive(Clone)]
struct ChatStreamLimit {
    active_chat_streams: Arc<AtomicUsize>,
    max_concurrent_chat_streams: usize,
}

impl ChatStreamLimit {
    fn new(max_concurrent_chat_streams: usize) -> Self {
        Self {
            active_chat_streams: Arc::new(AtomicUsize::new(0)),
            max_concurrent_chat_streams,
        }
    }

    fn try_acquire(&self) -> Option<ChatStreamPermit> {
        let mut active = self.active_chat_streams.load(Ordering::Relaxed);
        loop {
            if active >= self.max_concurrent_chat_streams {
                return None;
            }
            match self.active_chat_streams.compare_exchange_weak(
                active,
                active + 1,
                Ordering::AcqRel,
                Ordering::Relaxed,
            ) {
                Ok(_) => {
                    return Some(ChatStreamPermit {
                        active_chat_streams: Arc::clone(&self.active_chat_streams),
                    });
                }
                Err(updated) => active = updated,
            }
        }
    }
}

#[derive(Clone)]
struct ChatRequestRateLimiter {
    recent_chat_requests: Arc<Mutex<VecDeque<Instant>>>,
    max_chat_requests_per_window: usize,
    chat_request_window: Duration,
}

impl ChatRequestRateLimiter {
    fn new(max_chat_requests_per_window: usize, chat_request_window: Duration) -> Self {
        Self {
            recent_chat_requests: Arc::new(Mutex::new(VecDeque::new())),
            max_chat_requests_per_window,
            chat_request_window,
        }
    }

    fn allow(&self) -> bool {
        let now = Instant::now();
        let window_start = now - self.chat_request_window;
        let mut recent = self
            .recent_chat_requests
            .lock()
            .expect("chat request rate limiter poisoned");

        while recent
            .front()
            .copied()
            .is_some_and(|instant| instant < window_start)
        {
            recent.pop_front();
        }

        if recent.len() >= self.max_chat_requests_per_window {
            return false;
        }

        recent.push_back(now);
        true
    }
}

pub struct ChatStreamPermit {
    active_chat_streams: Arc<AtomicUsize>,
}

impl Drop for ChatStreamPermit {
    fn drop(&mut self) {
        self.active_chat_streams.fetch_sub(1, Ordering::AcqRel);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allow_chat_request_enforces_fixed_window_limit() {
        let limiter = ChatRequestRateLimiter::new(
            DEFAULT_MAX_CHAT_REQUESTS_PER_WINDOW,
            DEFAULT_CHAT_REQUEST_WINDOW,
        );
        for _ in 0..DEFAULT_MAX_CHAT_REQUESTS_PER_WINDOW {
            assert!(limiter.allow());
        }
        assert!(!limiter.allow());
    }

    #[test]
    fn try_acquire_chat_stream_enforces_concurrency_limit() {
        let limiter = ChatStreamLimit::new(DEFAULT_MAX_CONCURRENT_CHAT_STREAMS);
        let mut permits = Vec::new();
        for _ in 0..DEFAULT_MAX_CONCURRENT_CHAT_STREAMS {
            permits.push(
                limiter
                    .try_acquire()
                    .expect("permit within concurrency limit"),
            );
        }
        assert!(limiter.try_acquire().is_none());
        drop(permits.pop());
        assert!(limiter.try_acquire().is_some());
    }
}
