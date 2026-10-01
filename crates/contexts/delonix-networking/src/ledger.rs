//! The step ledger of a network apply (ADR-0059 D4).
//!
//! Each step is written to the ledger **before** it runs and settled after,
//! through the record's own store. A process killed mid-way leaves a step
//! that was opened and never settled: the next plan sees it and the next
//! apply reconciles it — adopting what the dead process staged on the
//! provider — instead of resending blindly. The shape is ADR-0049 slice 1's
//! task ledger.
//!
//! Plain data: nothing here does I/O. The caller saves the record after
//! every `open` and every `settle`.

use serde::{Deserialize, Serialize};

/// Where a step stands.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepState {
    /// Opened, and the process did not live to settle it — or it is running.
    Submitted,
    Done,
    Failed(String),
}

/// One step of an apply or a teardown.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Step {
    pub id: u32,
    /// `ensure_alias`, `ensure_rule`, `remove_rule`, `remove_alias`, `commit`.
    pub op: String,
    /// The alias name or the rule description; empty for `commit`.
    #[serde(default)]
    pub target: String,
    pub state: StepState,
}

/// The steps of the last apply or teardown of one document.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StepLedger {
    #[serde(default)]
    pub steps: Vec<Step>,
    /// Set by [`finish`](Self::finish) once the run's last step is settled.
    /// A process killed BETWEEN two steps leaves every step `Done` and
    /// nothing open: only this says the run did not reach its end.
    #[serde(default)]
    pub finished: bool,
    /// The provider's ids of the objects a teardown was about to delete,
    /// saved before the first deletion: a deletion staged by a process that
    /// died is recognized by its id, since the object is no longer there to
    /// carry a mark.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub removing: Vec<String>,
}

impl StepLedger {
    /// Opens a step and returns its id. Save the record before running it.
    pub fn open(&mut self, op: &str, target: &str) -> u32 {
        let id = self.steps.len() as u32 + 1;
        self.steps.push(Step {
            id,
            op: op.to_string(),
            target: target.to_string(),
            state: StepState::Submitted,
        });
        id
    }

    /// Settles a step with how it ended. Save the record after.
    pub fn settle(&mut self, id: u32, outcome: Result<(), String>) {
        if let Some(step) = self.steps.iter_mut().find(|s| s.id == id) {
            step.state = match outcome {
                Ok(()) => StepState::Done,
                Err(why) => StepState::Failed(why),
            };
        }
    }

    /// Marks the run as having reached its end. Save the record after.
    pub fn finish(&mut self) {
        self.finished = true;
    }

    /// The first step that did not end well: opened and never settled (the
    /// process died there), or failed.
    pub fn unfinished(&self) -> Option<&Step> {
        self.steps.iter().find(|s| s.state != StepState::Done)
    }

    /// Whether the last run stopped before its end: a step that did not end
    /// well, or steps done and no [`finish`](Self::finish). A ledger with no
    /// step at all (a record from before ledgers, a run that had not started)
    /// is not an interruption.
    pub fn is_interrupted(&self) -> bool {
        !self.steps.is_empty() && (!self.finished || self.unfinished().is_some())
    }

    /// The unfinished step as one line for a plan: what it was and how many
    /// steps before it were done.
    pub fn interruption(&self) -> Option<String> {
        if !self.is_interrupted() {
            return None;
        }
        let done = self
            .steps
            .iter()
            .filter(|s| s.state == StepState::Done)
            .count();
        let Some(step) = self.unfinished() else {
            return Some(format!(
                "interrupted: stopped after {done} step(s) done, before the next one"
            ));
        };
        let how = match &step.state {
            StepState::Failed(why) => format!("failed ({why})"),
            _ => "did not finish".to_string(),
        };
        let what = if step.target.is_empty() {
            step.op.clone()
        } else {
            format!("{} '{}'", step.op, step.target)
        };
        Some(format!(
            "interrupted: step {} ({what}) {how}, after {done} step(s) done",
            step.id
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_run_that_settles_every_step_is_finished() {
        let mut l = StepLedger::default();
        let a = l.open("ensure_rule", "web#1");
        l.settle(a, Ok(()));
        let c = l.open("commit", "");
        l.settle(c, Ok(()));
        l.finish();
        assert!(!l.is_interrupted());
        assert_eq!(l.interruption(), None);
    }

    /// Measured live (ADR-0059 F4c): a `kill -9` landed after step 2 was
    /// settled and before step 3 was opened. Every step read `done`, and the
    /// run had created 2 of 6 rules.
    #[test]
    fn a_run_killed_between_two_steps_is_an_interruption() {
        let mut l = StepLedger::default();
        for target in ["web#1", "web#2"] {
            let id = l.open("ensure_rule", target);
            l.settle(id, Ok(()));
        }
        assert_eq!(l.unfinished(), None);
        assert!(l.is_interrupted());
        assert_eq!(
            l.interruption().as_deref(),
            Some("interrupted: stopped after 2 step(s) done, before the next one")
        );
    }

    /// A process killed between `open` and `settle` leaves `Submitted` on
    /// disk: that is the step the next plan names.
    #[test]
    fn a_step_opened_and_never_settled_is_the_interruption() {
        let mut l = StepLedger::default();
        let a = l.open("ensure_rule", "web#1");
        l.settle(a, Ok(()));
        l.open("ensure_rule", "web#2");
        let text = serde_json::to_string(&l).unwrap();
        let back: StepLedger = serde_json::from_str(&text).unwrap();
        assert_eq!(back, l);
        assert_eq!(
            back.interruption().as_deref(),
            Some("interrupted: step 2 (ensure_rule 'web#2') did not finish, after 1 step(s) done")
        );
    }

    #[test]
    fn a_failed_step_says_why() {
        let mut l = StepLedger::default();
        let c = l.open("commit", "");
        l.settle(c, Err("the appliance refused".into()));
        assert_eq!(
            l.interruption().as_deref(),
            Some(
                "interrupted: step 1 (commit) failed (the appliance refused), after 0 step(s) done"
            )
        );
    }

    /// A record written before the ledger existed has none: it reads as a
    /// finished run, never as an interruption.
    #[test]
    fn a_record_without_a_ledger_is_finished() {
        let l: StepLedger = serde_json::from_str("{}").unwrap();
        assert!(!l.is_interrupted());
    }
}
