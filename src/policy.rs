//! Service lifecycle policy.
use std::time::Duration;

/// Supervisor reaction to a service exit.
///
/// [`ExitAction`] is policy, not a service self-directive:
/// a service reports a [`ServiceOutcome`](crate::ServiceOutcome), then the
/// supervisor chooses whether to ignore it, restart the task, or begin shutting
/// the system down.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExitAction {
    /// Accept the exit and leave the rest of the system running.
    Ignore,
    /// Spawn the same service again, optionally after policy backoff.
    Restart,
    /// Cancel the root token and begin coordinated supervisor shutdown.
    Shutdown,
}

/// Supervisor reaction to a service error when a retry path is not taken.
///
/// [`ErrorAction`] intentionally omits restart semantics. Restart belongs to
/// [`RestartPolicy`], which makes contradictory states like “never restart, but
/// restart when exhausted” impossible to represent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrorAction {
    /// Accept the failure and leave the rest of the system running.
    Ignore,
    /// Cancel the root token and begin coordinated supervisor shutdown.
    Shutdown,
}

/// Restart behavior applied when a service exits with
/// [`ServiceOutcome::Error`](crate::ServiceOutcome::Error).
///
/// The enum shape keeps the policy honest:
/// - [`RestartPolicy::Never`] never retries
/// - [`RestartPolicy::Always`] always retries
/// - [`RestartPolicy::Attempts`] retries a bounded number of times, then falls
///   back to [`ErrorAction`]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RestartPolicy {
    /// Do not retry the service after an error.
    Never { on_error: ErrorAction },
    /// Retry indefinitely with a fixed backoff between attempts.
    Always { backoff: Duration },
    /// Retry up to `max_restarts` times, then use `when_exhausted`.
    Attempts {
        max_restarts: usize,
        backoff: Duration,
        when_exhausted: ErrorAction,
    },
}

impl RestartPolicy {
    /// Creates a retry policy with a bounded number of retries and a fixed
    /// backoff between attempts, followed by `when_exhausted`.
    pub fn attempts(max_restarts: usize, backoff: Duration, when_exhausted: ErrorAction) -> Self {
        Self::Attempts {
            max_restarts,
            backoff,
            when_exhausted,
        }
    }

    /// Disables retries and immediately uses `on_error`.
    pub fn never(on_error: ErrorAction) -> Self {
        Self::Never { on_error }
    }

    /// Retries indefinitely with a fixed backoff between attempts.
    pub fn always(backoff: Duration) -> Self {
        Self::Always { backoff }
    }

    /// Returns the bounded retry count when this policy is
    /// [`RestartPolicy::Attempts`].
    pub fn max_restarts(&self) -> Option<usize> {
        match self {
            Self::Never { .. } | Self::Always { .. } => None,
            Self::Attempts { max_restarts, .. } => Some(*max_restarts),
        }
    }

    /// Returns the retry backoff when this policy can restart.
    pub fn backoff(&self) -> Option<Duration> {
        match self {
            Self::Never { .. } => None,
            Self::Always { backoff } | Self::Attempts { backoff, .. } => Some(*backoff),
        }
    }

    /// Returns the terminal fallback action once retries are unavailable.
    pub fn on_error(&self) -> Option<ErrorAction> {
        match self {
            Self::Never { on_error } => Some(*on_error),
            Self::Always { .. } => None,
            Self::Attempts { when_exhausted, .. } => Some(*when_exhausted),
        }
    }

    pub(crate) fn action(&self, restarts: usize) -> FailureAction {
        match self {
            Self::Never { on_error } => FailureAction::Terminal(*on_error),
            Self::Always { backoff } => FailureAction::Restart { backoff: *backoff },
            Self::Attempts {
                max_restarts,
                backoff,
                when_exhausted,
            } => {
                if restarts < *max_restarts {
                    FailureAction::Restart { backoff: *backoff }
                } else {
                    FailureAction::Terminal(*when_exhausted)
                }
            },
        }
    }
}

/// Per-service exit policy owned by the supervisor.
///
/// [`ServicePolicy`] separates two independent decisions:
/// - how to react when a task returns
///   [`ServiceOutcome::Completed`](crate::ServiceOutcome::Completed)
/// - how to react when a task returns
///   [`ServiceOutcome::Error`](crate::ServiceOutcome::Error)
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServicePolicy {
    on_completed: ExitAction,
    restart: RestartPolicy,
}

impl ServicePolicy {
    /// Creates a policy for one registered service.
    pub fn new(on_completed: ExitAction, restart: RestartPolicy) -> Self {
        Self {
            on_completed,
            restart,
        }
    }

    pub fn on_completed(&self) -> ExitAction {
        self.on_completed
    }

    pub fn restart(&self) -> &RestartPolicy {
        &self.restart
    }
}

impl Default for ServicePolicy {
    fn default() -> Self {
        Self {
            on_completed: ExitAction::Ignore,
            restart: RestartPolicy::never(ErrorAction::Shutdown),
        }
    }
}

/// Internal supervisor decision after a service failure.
pub(crate) enum FailureAction {
    Restart { backoff: Duration },
    Terminal(ErrorAction),
}
