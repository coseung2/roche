use thiserror::Error;

use crate::models::TaskStatus;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum TransitionError {
    #[error("invalid task transition: {from:?} -> {to:?}")]
    Invalid { from: TaskStatus, to: TaskStatus },
}

pub fn can_transition(from: TaskStatus, to: TaskStatus) -> bool {
    use TaskStatus::*;

    matches!(
        (from, to),
        (Queued, Preparing)
            | (Queued, Cancelled)
            | (Preparing, RunningCodex)
            | (Preparing, Failed)
            | (Preparing, Cancelled)
            | (RunningCodex, Verifying)
            | (RunningCodex, Failed)
            | (RunningCodex, Cancelled)
            | (Verifying, NeedsReview)
            | (Verifying, Failed)
            | (NeedsReview, RunningCodex)
            | (NeedsReview, Completed)
            | (NeedsReview, Cancelled)
            | (Failed, Preparing)
            | (Failed, Cancelled)
    )
}

pub fn transition(from: TaskStatus, to: TaskStatus) -> Result<TaskStatus, TransitionError> {
    can_transition(from, to)
        .then_some(to)
        .ok_or(TransitionError::Invalid { from, to })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn happy_path_requires_verification_and_review() {
        let mut state = TaskStatus::Queued;
        for next in [
            TaskStatus::Preparing,
            TaskStatus::RunningCodex,
            TaskStatus::Verifying,
            TaskStatus::NeedsReview,
            TaskStatus::Completed,
        ] {
            state = transition(state, next).unwrap();
        }
        assert_eq!(state, TaskStatus::Completed);
    }

    #[test]
    fn codex_done_cannot_complete_task_directly() {
        assert_eq!(
            transition(TaskStatus::RunningCodex, TaskStatus::Completed),
            Err(TransitionError::Invalid {
                from: TaskStatus::RunningCodex,
                to: TaskStatus::Completed,
            })
        );
    }
}
