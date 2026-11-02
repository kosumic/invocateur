//! Trigger scheduling is independent of any source, prerequisite, or task type.
use anyhow::{Result, ensure};

pub mod github;

/// A source owns its prerequisite checks, persistent state, and task dispatch.
/// A poll failure skips this source for the current cycle, not other sources.
pub trait Trigger {
    fn name(&self) -> &str;
    fn poll(&mut self) -> Result<()>;
}

pub fn poll_all(triggers: &mut [Box<dyn Trigger + '_>]) -> Result<()> {
    let mut failed = 0;
    for trigger in triggers {
        if let Err(error) = trigger.poll() {
            eprintln!("trigger {}: {error:#}", trigger.name());
            failed += 1;
        }
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
        let mut triggers: Vec<Box<dyn Trigger>> = vec![
            Box::new(TestTrigger {
                fails: true,
                calls: unavailable.clone(),
            }),
            Box::new(TestTrigger {
                fails: false,
                calls: healthy.clone(),
            }),
        ];
        for expected in 1..=2 {
            assert!(poll_all(&mut triggers).is_err());
            assert_eq!(unavailable.get(), expected);
            assert_eq!(healthy.get(), expected);
        }
    }
}
