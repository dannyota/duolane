pub(super) fn idle_pool_bound(soft_limit: Option<u64>) -> usize {
    let Some(limit) = soft_limit else { return 2048 };
    let (intercepted, tunnels) = crate::intercept::pool_sizes(Some(limit));
    let spare = limit
        .saturating_sub(192)
        .saturating_sub(2 * (intercepted + tunnels) as u64);
    (spare / 2).min(2048) as usize
}

pub(super) static IDLE_POOL_BOUND: once_cell::sync::Lazy<usize> =
    once_cell::sync::Lazy::new(|| {
        #[cfg(unix)]
        let limit = rustix::process::getrlimit(rustix::process::Resource::Nofile).current;
        #[cfg(not(unix))]
        let limit = None;
        idle_pool_bound(limit)
    });
