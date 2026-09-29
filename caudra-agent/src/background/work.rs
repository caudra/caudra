use crate::{TaskCard, workflow::WorkflowHandle};

use super::{BackgroundTasks, deliverable, eligible};

#[derive(Debug, Default, PartialEq, Eq)]
pub struct SessionWork {
    pub running: bool,
    pub settling: bool,
    pub unavailable: bool,
}

impl SessionWork {
    pub fn capture(
        background: Option<&BackgroundTasks>,
        workflow: Option<&WorkflowHandle>,
        foreground_tasks: usize,
    ) -> Self {
        let mut work = background.map(BackgroundTasks::work).unwrap_or_default();
        work.running |= foreground_tasks > 0;
        if let Some(workflow) = workflow {
            let workflow = workflow.work();
            work.running |= workflow.running;
            work.settling |= workflow.settling;
            work.unavailable |= workflow.unavailable;
        }
        work
    }

    pub fn pending(&self) -> bool {
        self.running || self.settling || self.unavailable
    }
}

impl BackgroundTasks {
    pub fn resident_status(&self, task_id: &str) -> Option<TaskCard> {
        self.lock()
            .records
            .values()
            .filter(|record| record.task_id == task_id)
            .max_by_key(|record| record.sequence)
            .map(TaskCard::from)
    }

    pub fn resident_invocation_status(
        &self,
        task_id: &str,
        invocation_id: &str,
    ) -> Option<TaskCard> {
        self.lock()
            .records
            .get(invocation_id)
            .filter(|record| record.task_id == task_id)
            .map(TaskCard::from)
    }

    pub fn work(&self) -> SessionWork {
        let state = self.lock();
        SessionWork {
            running: state.records.values().any(|record| record.active()),
            settling: !state.admitting.is_empty()
                || !state.drivers.is_empty()
                || state.pending_stops > 0
                || state.transition.is_some()
                || !state.claims.is_empty()
                || self.0.shells.pending()
                || (state.open
                    && state.records.values().any(|record| {
                        eligible(record, state.generation)
                            && record.events.iter().any(|event| deliverable(record, event))
                    })),
            unavailable: state.failure.is_some(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::SessionWork;
    use test_case::test_case;

    #[test_case(false, false, false, false; "idle")]
    #[test_case(true, false, false, true; "running")]
    #[test_case(false, true, false, true; "delivery")]
    #[test_case(false, false, true, true; "unavailable")]
    fn readiness_requires_settled_work(
        running: bool,
        settling: bool,
        unavailable: bool,
        pending: bool,
    ) {
        assert_eq!(
            SessionWork {
                running,
                settling,
                unavailable
            }
            .pending(),
            pending
        );
    }
}
