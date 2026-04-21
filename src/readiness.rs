//! Startup readiness primitives.
use std::{
    fmt,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc,
    },
};

use tokio::sync::watch;

/// Aggregate startup readiness reported by a running
/// [`Supervisor`](crate::Supervisor).
///
/// Readiness is deliberately narrower than liveness: it only answers whether
/// all services registered with [`ReadinessMode::Explicit`] have crossed their
/// startup gate at least once, either by signaling readiness or completing
/// successfully.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SupervisorReadiness {
    /// At least one startup-gated service has not crossed its startup gate.
    Pending,
    /// Every startup-gated service has crossed its startup gate.
    Ready,
}

/// Per-registration startup readiness behavior.
///
/// The default is [`ReadinessMode::Immediate`], which keeps ordinary services
/// out of the readiness path. Use [`ReadinessMode::Explicit`] when the
/// supervisor should wait for the service to finish meaningful startup work by
/// either calling [`ReadySignal::mark_ready`] or returning
/// [`ServiceOutcome::Completed`](crate::ServiceOutcome::Completed).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ReadinessMode {
    /// The service counts as ready as soon as it is spawned.
    #[default]
    Immediate,
    /// The service must either call [`ReadySignal::mark_ready`] or complete
    /// successfully before any other terminal outcome.
    Explicit,
}

/// Service-local handle used to declare startup readiness.
///
/// The handle is intentionally tiny and idempotent. It does not expose
/// snapshots, failure marking, partitions, or policy decisions; those remain
/// supervisor-owned concepts.
#[derive(Clone)]
pub struct ReadySignal {
    inner: ReadySignalInner,
}

impl ReadySignal {
    pub(crate) fn immediate() -> Self {
        Self {
            inner: ReadySignalInner::Immediate,
        }
    }

    pub(crate) fn explicit(tracker: ReadinessTracker, index: usize) -> Self {
        Self {
            inner: ReadySignalInner::Explicit { tracker, index },
        }
    }

    /// Marks this service ready if it participates in explicit startup
    /// readiness. Calling this more than once is harmless.
    pub fn mark_ready(&self) {
        match &self.inner {
            ReadySignalInner::Immediate => {},
            ReadySignalInner::Explicit { tracker, index } => {
                tracker.mark_ready(*index);
            },
        }
    }
}

impl fmt::Debug for ReadySignal {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.inner {
            ReadySignalInner::Immediate => formatter.write_str("ReadySignal::Immediate"),
            ReadySignalInner::Explicit { index, .. } => formatter
                .debug_struct("ReadySignal::Explicit")
                .field("index", index)
                .finish_non_exhaustive(),
        }
    }
}

#[derive(Clone)]
enum ReadySignalInner {
    Immediate,
    Explicit {
        tracker: ReadinessTracker,
        index: usize,
    },
}

#[derive(Clone)]
pub(crate) struct ReadinessTracker {
    inner: Arc<ReadinessInner>,
}

impl ReadinessTracker {
    pub(crate) fn new(modes: impl IntoIterator<Item = ReadinessMode>) -> Self {
        let mut pending = 0;
        let members = modes
            .into_iter()
            .map(|mode| match mode {
                ReadinessMode::Immediate => MemberReadiness::Immediate,
                ReadinessMode::Explicit => {
                    pending += 1;
                    MemberReadiness::Explicit {
                        ready: AtomicBool::new(false),
                    }
                },
            })
            .collect::<Vec<_>>();
        let state = readiness_state(pending);
        let (sender, _) = watch::channel(state);

        Self {
            inner: Arc::new(ReadinessInner {
                members,
                pending: AtomicUsize::new(pending),
                sender,
            }),
        }
    }

    pub(crate) fn signal(&self, index: usize, mode: ReadinessMode) -> ReadySignal {
        match mode {
            ReadinessMode::Immediate => ReadySignal::immediate(),
            ReadinessMode::Explicit => ReadySignal::explicit(self.clone(), index),
        }
    }

    pub(crate) fn subscribe(&self) -> watch::Receiver<SupervisorReadiness> {
        self.inner.sender.subscribe()
    }

    pub(crate) fn state(&self) -> SupervisorReadiness {
        readiness_state(self.inner.pending.load(Ordering::Acquire))
    }

    pub(crate) fn is_ready(&self, index: usize) -> bool {
        self.inner
            .members
            .get(index)
            .is_some_and(MemberReadiness::is_ready)
    }

    pub(crate) fn mark_ready(&self, index: usize) {
        let Some(member) = self.inner.members.get(index) else {
            return;
        };

        if member.mark_ready() {
            let previous = self.inner.pending.fetch_sub(1, Ordering::AcqRel);
            if previous == 1 {
                let _ = self.inner.sender.send(SupervisorReadiness::Ready);
            }
        }
    }
}

struct ReadinessInner {
    members: Vec<MemberReadiness>,
    pending: AtomicUsize,
    sender: watch::Sender<SupervisorReadiness>,
}

enum MemberReadiness {
    Immediate,
    Explicit { ready: AtomicBool },
}

impl MemberReadiness {
    pub(crate) fn is_ready(&self) -> bool {
        match self {
            Self::Immediate => true,
            Self::Explicit { ready } => ready.load(Ordering::Acquire),
        }
    }

    pub(crate) fn mark_ready(&self) -> bool {
        match self {
            Self::Immediate => false,
            Self::Explicit { ready } => ready
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_ok(),
        }
    }
}

fn readiness_state(pending: usize) -> SupervisorReadiness {
    if pending == 0 {
        SupervisorReadiness::Ready
    } else {
        SupervisorReadiness::Pending
    }
}
