//! Trigger scheduling is independent of any source, prerequisite, or task type.
use anyhow::{Result, ensure};
use std::time::{Duration, Instant};

pub mod ci;
pub mod github;
pub mod schedule;

/// A source owns its prerequisite checks, persistent state, and task dispatch.
/// A poll failure skips this source for the current cycle, not other sources.
pub trait Trigger {
    fn name(&self) -> &str;
    fn poll(&mut self) -> Result<()>;
}

pub struct Registration<'a> {
    trigger: Box<dyn Trigger + 'a>,
    interval: Duration,
    next_poll: Instant,
}
impl<'a> Registration<'a> {
    pub fn new(trigger: Box<dyn Trigger + 'a>, interval: Duration) -> Self {
        Self {
            trigger,
            interval,
            next_poll: Instant::now(),
        }
    }
}

pub fn next_delay(triggers: &[Registration<'_>]) -> Duration {
    triggers
        .iter()
        .map(|entry| entry.next_poll.saturating_duration_since(Instant::now()))
        .min()
        .unwrap_or(Duration::from_secs(60))
}

pub fn poll_all(triggers: &mut [Registration<'_>]) -> Result<()> {
    let mut failed = 0;
    for entry in triggers {
        if Instant::now() < entry.next_poll {
            continue;
        }
        if let Err(error) = entry.trigger.poll() {
            eprintln!("trigger {}: {error:#}", entry.trigger.name());
            failed += 1;
        }
        entry.next_poll = Instant::now() + entry.interval;
    }
    ensure!(failed == 0, "{failed} trigger(s) could not be polled");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{cell::Cell, rc::Rc};

    struct TestTrigger {
        fails: bool,
        calls: Rc<Cell<u32>>,
    }
    impl Trigger for TestTrigger {
        fn name(&self) -> &str {
            "test"
        }
        fn poll(&mut self) -> Result<()> {
            self.calls.set(self.calls.get() + 1);
            ensure!(!self.fails, "prerequisite unavailable");
            Ok(())
        }
    }

    #[test]
    fn unavailable_trigger_does_not_block_others_and_is_retried_next_cycle() {
        let unavailable = Rc::new(Cell::new(0));
        let healthy = Rc::new(Cell::new(0));
        let mut triggers: Vec<_> = vec![
            Registration::new(
                Box::new(TestTrigger {
                    fails: true,
                    calls: unavailable.clone(),
                }),
                Duration::ZERO,
            ),
            Registration::new(
                Box::new(TestTrigger {
                    fails: false,
                    calls: healthy.clone(),
                }),
                Duration::ZERO,
            ),
        ];
        for expected in 1..=2 {
            assert!(poll_all(&mut triggers).is_err());
            assert_eq!(unavailable.get(), expected);
            assert_eq!(healthy.get(), expected);
        }
    }
}
