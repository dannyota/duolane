use crate::claude_accounts::profile_state::Roster;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

const REUSE: Duration = Duration::from_secs(300);

struct Job {
    receiver: mpsc::Receiver<Option<Roster>>,
    cancelled: Arc<AtomicBool>,
    handle: std::thread::JoinHandle<()>,
}

#[derive(Default)]
pub(super) struct Details {
    checked: Option<(Roster, Instant)>,
    job: Option<Job>,
}

impl Details {
    pub(super) fn request(&mut self, force: bool) {
        #[cfg(all(unix, not(test)))]
        self.request_with(force, crate::claude_accounts::monitor_details);
        #[cfg(any(not(unix), test))]
        let _ = force;
    }

    pub(super) fn request_with(
        &mut self,
        force: bool,
        check: impl FnOnce(&AtomicBool) -> Option<Roster> + Send + 'static,
    ) {
        let fresh = self
            .checked
            .as_ref()
            .is_some_and(|(_, at)| at.elapsed() < REUSE);
        if self.job.is_some() || (fresh && !force) {
            return;
        }
        let (sender, receiver) = mpsc::channel();
        let cancelled = Arc::new(AtomicBool::new(false));
        let flag = cancelled.clone();
        let spawned = std::thread::Builder::new()
            .name("account-details".into())
            .spawn(move || {
                let _ = sender.send(check(&flag));
            });
        if let Ok(handle) = spawned {
            self.job = Some(Job {
                receiver,
                cancelled,
                handle,
            });
        }
    }

    pub(super) fn poll(&mut self) {
        let Some(job) = &self.job else {
            return;
        };
        let result = match job.receiver.try_recv() {
            Err(mpsc::TryRecvError::Empty) => return,
            Ok(result) => result,
            Err(mpsc::TryRecvError::Disconnected) => None,
        };
        if let Some(job) = self.job.take() {
            let _ = job.handle.join();
        }
        if let Some(roster) = result {
            self.checked = Some((roster, Instant::now()));
        }
    }

    pub(super) fn checking(&self) -> bool {
        self.job.is_some()
    }

    pub(super) fn roster(&self, names: &[String]) -> Roster {
        match &self.checked {
            Some((roster, _)) => roster.aligned(names),
            None => Roster::unknown(names),
        }
    }

    pub(super) fn summary(&self) -> String {
        match (&self.checked, self.checking()) {
            (_, true) => "Accounts checking".into(),
            (Some((_, at)), false) => format!(
                "Accounts checked {} ago",
                super::cells::format_duration(at.elapsed())
            ),
            (None, false) => "Accounts not checked".into(),
        }
    }
}

impl Drop for Details {
    fn drop(&mut self) {
        if let Some(job) = self.job.take() {
            job.cancelled.store(true, Ordering::Relaxed);
            let _ = job.handle.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wait(details: &mut Details) {
        for _ in 0..500 {
            details.poll();
            if !details.checking() {
                return;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        panic!("check did not finish");
    }

    #[test]
    fn check_runs_once_at_a_time_and_reuses_a_fresh_result() {
        let mut details = Details::default();
        assert_eq!(details.summary(), "Accounts not checked");
        let (release, gate) = mpsc::channel::<()>();
        details.request_with(false, move |_| {
            let _ = gate.recv();
            Some(Roster::default())
        });
        assert_eq!(details.summary(), "Accounts checking");
        details.request_with(true, |_| panic!("second check started during a check"));
        release.send(()).unwrap();
        wait(&mut details);
        assert!(details.summary().starts_with("Accounts checked "));
        details.request_with(false, |_| panic!("fresh result was not reused"));
        assert!(!details.checking());
        details.request_with(true, |_| None);
        assert!(details.checking());
        wait(&mut details);
        assert!(details.summary().starts_with("Accounts checked "));
    }

    #[test]
    fn leaving_cancels_a_running_check_and_waits_for_it() {
        let mut details = Details::default();
        let finished = Arc::new(AtomicBool::new(false));
        let seen = finished.clone();
        details.request_with(false, move |cancelled| {
            while !cancelled.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(1));
            }
            seen.store(true, Ordering::Relaxed);
            None
        });
        drop(details);
        assert!(finished.load(Ordering::Relaxed));
    }
}
