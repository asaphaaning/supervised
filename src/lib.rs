#![doc = include_str!("../README.md")]

mod runtime;

pub mod policy;
pub mod readiness;
pub mod service;

pub use policy::{ErrorAction, ExitAction, RestartPolicy, ServicePolicy};
pub use readiness::{ReadinessMode, ReadySignal, SupervisorReadiness};
pub use runtime::{
    Context,
    Error,
    FromSupervisorState,
    Options,
    RunSummary,
    ServiceSummary,
    ShutdownCause,
    Supervisor,
    SupervisorBuilder,
};
pub use service::{
    service_fn,
    until_cancelled,
    when_ready,
    BoxFuture,
    FnService,
    ServiceError,
    ServiceExt,
    ServiceOutcome,
    SupervisedService,
    UntilCancelled,
    WhenReady,
};
