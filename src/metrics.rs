use std::sync::atomic::{AtomicUsize, Ordering};

pub static REQUESTS_TOTAL: AtomicUsize = AtomicUsize::new(0);
pub static AUTH_FAILURES: AtomicUsize = AtomicUsize::new(0);
pub static RATE_LIMITED: AtomicUsize = AtomicUsize::new(0);
pub static AUTH_SUCCESS: AtomicUsize = AtomicUsize::new(0);
pub static GATEWAY_ACCEPTED: AtomicUsize = AtomicUsize::new(0);
pub static UPSTREAM_SUCCESS: AtomicUsize = AtomicUsize::new(0);
pub static UPSTREAM_FAILURES: AtomicUsize = AtomicUsize::new(0);


pub fn metrics_text() -> String {
    format!(
        "requests_total {}\n\
        auth_failures {}\n\
        auth_success {}\n\
        rate_limited {}\n\
        gateway_accepted {}\n\
        upstream_success {}\n\
        upstream_failures {}\n",
        REQUESTS_TOTAL.load(Ordering::Relaxed),
        AUTH_FAILURES.load(Ordering::Relaxed),
        AUTH_SUCCESS.load(Ordering::Relaxed),
        RATE_LIMITED.load(Ordering::Relaxed),
        GATEWAY_ACCEPTED.load(Ordering::Relaxed),
        UPSTREAM_SUCCESS.load(Ordering::Relaxed),
        UPSTREAM_FAILURES.load(Ordering::Relaxed),
    )
}