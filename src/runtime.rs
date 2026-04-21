//! Runtime and builder for supervised services.
use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, Instant},
};

use thiserror::Error as ThisError;
use tokio::{
    sync::watch,
    task::{Id as TaskId, JoinError, JoinSet},
    time::{sleep, timeout},
};
use tokio_util::sync::CancellationToken;
use tracing::instrument;

use crate::{
    policy::{ErrorAction, ExitAction, FailureAction, RestartPolicy, ServicePolicy},
    readiness::{ReadinessMode, ReadinessTracker, ReadySignal, SupervisorReadiness},
    service::{BoxFuture, ServiceOutcome, SupervisedService},
};

/// Extracts a service-local context from the supervisor's root state.
///
/// This follows the same broad idea as Axum's state extraction: the builder
/// owns one root state value, while each service asks for either the full state
/// or a typed subset of it.
///
/// In practice, registration works like this:
/// - [`SupervisorBuilder::new`] stores the root state `S`
/// - [`SupervisorBuilder::add`] accepts a [`SupervisedService`]
/// - the builder resolves `T::Context` from `S` through [`FromSupervisorState`]
/// - the resolved value is then wrapped in [`Context`] each time the service
///   runs
///
/// The blanket identity implementation means a service can ask for the full
/// root state directly whenever that is the clearest shape.
pub trait FromSupervisorState<S>: Sized {
    /// Projects `Self` out of the root supervisor state.
    fn from_state(state: &S) -> Self;
}

impl<S> FromSupervisorState<S> for S
where
    S: Clone,
{
    fn from_state(state: &S) -> Self {
        state.clone()
    }
}

/// Supervisor-owned runtime context for one service execution.
///
/// Every supervised task receives:
/// - a [`CancellationToken`] controlled by the supervisor
/// - a typed service-local payload `C`
///
/// The token makes shutdown cooperative and explicit, while the payload keeps
/// service dependencies local instead of forcing a monolithic app context.
#[derive(Clone, Debug)]
pub struct Context<C> {
    token: CancellationToken,
    readiness: ReadySignal,
    ctx: C,
}

impl<C> Context<C> {
    pub fn new(token: CancellationToken, ctx: C) -> Self {
        Self {
            token,
            readiness: ReadySignal::immediate(),
            ctx,
        }
    }

    pub(crate) fn with_readiness(token: CancellationToken, readiness: ReadySignal, ctx: C) -> Self {
        Self {
            token,
            readiness,
            ctx,
        }
    }

    pub fn token(&self) -> &CancellationToken {
        &self.token
    }

    pub fn readiness(&self) -> &ReadySignal {
        &self.readiness
    }

    pub fn ctx(&self) -> &C {
        &self.ctx
    }

    pub fn into_inner(self) -> C {
        self.ctx
    }

    pub fn map<D>(self, map: impl FnOnce(C) -> D) -> Context<D> {
        Context {
            token: self.token,
            readiness: self.readiness,
            ctx: map(self.ctx),
        }
    }
}

/// Explicit registration options for [`SupervisorBuilder::add_with_options`].
///
/// This keeps optional dimensions as data instead of multiplying builder
/// methods. Prefer service adapters like [`when_ready`](crate::when_ready) and
/// [`until_cancelled`](crate::until_cancelled) when they describe service
/// shape; use [`Options`] when registration policy itself needs to be explicit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Options {
    policy: Option<ServicePolicy>,
    readiness: Option<ReadinessMode>,
}

impl Options {
    /// Creates empty registration options.
    pub fn new() -> Self {
        Self {
            policy: None,
            readiness: None,
        }
    }

    /// Uses `policy` for this registration instead of the builder defaults.
    pub fn policy(mut self, policy: ServicePolicy) -> Self {
        self.policy = Some(policy);
        self
    }

    /// Overrides service-derived readiness for this registration.
    pub fn readiness(mut self, readiness: ReadinessMode) -> Self {
        self.readiness = Some(readiness);
        self
    }
}

impl Default for Options {
    fn default() -> Self {
        Self::new()
    }
}

/// Reason the supervisor stopped running.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ShutdownCause {
    /// Every service finished and nothing requested shutdown.
    Completed,
    /// Shutdown was requested externally by the embedding runtime.
    Requested,
    /// Shutdown was triggered by an operating-system signal.
    Signal,
    /// A service returned [`ServiceOutcome::RequestedShutdown`].
    ServiceRequested { service: &'static str },
    /// A service failed and policy escalated that failure into shutdown.
    FatalService { service: &'static str },
    /// A startup-gated service ended before crossing its startup gate.
    ReadinessFailed { service: &'static str },
}

/// Terminal summary for one registered service.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServiceSummary {
    name: &'static str,
    outcome: ServiceOutcome,
    restarts: usize,
}

impl ServiceSummary {
    pub fn name(&self) -> &'static str {
        self.name
    }

    pub fn outcome(&self) -> &ServiceOutcome {
        &self.outcome
    }

    pub fn restarts(&self) -> usize {
        self.restarts
    }
}

/// Final supervisor result once [`Supervisor::run`] finishes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunSummary {
    shutdown_cause: ShutdownCause,
    readiness: SupervisorReadiness,
    services: Vec<ServiceSummary>,
}

impl RunSummary {
    /// Creates a final run summary. Most callers should receive this from
    /// [`Supervisor::run`] rather than constructing it directly.
    pub fn new(
        shutdown_cause: ShutdownCause,
        readiness: SupervisorReadiness,
        services: Vec<ServiceSummary>,
    ) -> Self {
        Self {
            shutdown_cause,
            readiness,
            services,
        }
    }

    pub fn shutdown_cause(&self) -> &ShutdownCause {
        &self.shutdown_cause
    }

    pub fn readiness(&self) -> SupervisorReadiness {
        self.readiness
    }

    pub fn services(&self) -> &[ServiceSummary] {
        &self.services
    }

    pub fn service(&self, name: &str) -> Option<&ServiceSummary> {
        self.services.iter().find(|service| service.name == name)
    }
}

/// Builder for a typed [`Supervisor`] runtime backed by root state `S`.
///
/// The registration vocabulary is intentionally small:
/// - [`SupervisorBuilder::add`] registers a service with the default policy
/// - [`SupervisorBuilder::add_with_options`] registers a service with explicit
///   [`Options`]
///
/// This keeps the builder focused on composition, while helpers like
/// [`service_fn`](crate::service_fn), [`when_ready`](crate::when_ready), and
/// [`until_cancelled`](crate::until_cancelled) describe service shape.
pub struct SupervisorBuilder<S> {
    state: S,
    registrations: Vec<Registration>,
    shutdown_timeout: Duration,
    default_restart_policy: RestartPolicy,
}

impl<S> SupervisorBuilder<S> {
    /// Creates a builder with root state `state`, a 5 second shutdown timeout,
    /// and a default restart policy of
    /// [`RestartPolicy::never`]`([`ErrorAction::Shutdown`])`.
    ///
    /// Every later registration will project its own context from this root
    /// state through [`FromSupervisorState`].
    pub fn new(state: S) -> Self {
        Self {
            state,
            registrations: Vec::new(),
            shutdown_timeout: Duration::from_secs(5),
            default_restart_policy: RestartPolicy::never(ErrorAction::Shutdown),
        }
    }

    /// Sets the grace period used before lingering tasks are aborted.
    pub fn shutdown_timeout(mut self, timeout: Duration) -> Self {
        self.shutdown_timeout = timeout;
        self
    }

    /// Sets the restart policy used by [`SupervisorBuilder::add`] when no
    /// explicit [`ServicePolicy`] is supplied.
    pub fn default_restart_policy(mut self, restart: RestartPolicy) -> Self {
        self.default_restart_policy = restart;
        self
    }

    /// Registers a [`SupervisedService`] using context extracted from the root
    /// state via [`FromSupervisorState`], the builder's default
    /// [`RestartPolicy`], and [`ExitAction::Ignore`] for normal completion.
    ///
    /// This is the default registration path for both concrete services and
    /// [`service_fn`](crate::service_fn) values.
    pub fn add<T>(self, service: T) -> Self
    where
        T: SupervisedService,
        T::Context: FromSupervisorState<S>,
    {
        self.add_with_options(service, Options::new())
    }

    /// Registers a [`SupervisedService`] with explicit [`Options`].
    ///
    /// This is the extension point for optional registration behavior that
    /// should not become another builder method dimension.
    pub fn add_with_options<T>(mut self, service: T, options: Options) -> Self
    where
        T: SupervisedService,
        T::Context: FromSupervisorState<S>,
    {
        let policy = options.policy.unwrap_or_else(|| {
            ServicePolicy::new(ExitAction::Ignore, self.default_restart_policy.clone())
        });
        let readiness = options.readiness.unwrap_or_else(|| service.readiness());
        let ctx = T::Context::from_state(&self.state);
        self.registrations
            .push(Registration::new(service, ctx, policy, readiness));
        self
    }

    /// Finalizes registration and returns a runnable [`Supervisor`].
    pub fn build(self) -> Supervisor {
        let readiness = ReadinessTracker::new(
            self.registrations
                .iter()
                .map(|registration| registration.readiness),
        );

        Supervisor {
            registrations: self.registrations,
            shutdown_timeout: self.shutdown_timeout,
            readiness,
        }
    }
}

/// Runtime that owns service lifecycle, restart policy, and coordinated
/// shutdown.
pub struct Supervisor {
    registrations: Vec<Registration>,
    shutdown_timeout: Duration,
    readiness: ReadinessTracker,
}

impl Supervisor {
    /// Subscribes to aggregate startup readiness.
    ///
    /// The receiver starts at [`SupervisorReadiness::Ready`] when no service is
    /// startup-gated, or [`SupervisorReadiness::Pending`] until every gated
    /// service crosses its startup gate.
    pub fn readiness(&self) -> watch::Receiver<SupervisorReadiness> {
        self.readiness.subscribe()
    }

    /// Runs all registered services until they complete or policy triggers
    /// shutdown, then returns a final [`RunSummary`].
    #[instrument(skip_all)]
    pub async fn run(self) -> Result<RunSummary, Error> {
        let Self {
            registrations,
            shutdown_timeout,
            readiness,
        } = self;

        if registrations.is_empty() {
            return Err(Error::NoServices);
        }

        let root = CancellationToken::new();
        let mut tasks = JoinSet::new();
        let mut running = HashMap::new();
        let mut states = registrations
            .into_iter()
            .map(ServiceState::new)
            .collect::<Vec<_>>();

        for (index, state) in states.iter().enumerate() {
            spawn_service(
                &mut tasks,
                &mut running,
                &state.registration,
                index,
                root.clone(),
                readiness.signal(index, state.registration.readiness),
                None,
            );
        }

        let mut shutdown = None;

        loop {
            let Some(joined) = tasks.join_next_with_id().await else {
                let cause = shutdown.unwrap_or(ShutdownCause::Completed);
                return Ok(RunSummary::new(
                    cause,
                    readiness.state(),
                    summarize(&states),
                ));
            };

            let result = join_result(joined, &mut running, &mut states)?;
            let state = state_mut(&mut states, result.index)?;
            state.last_outcome = result.outcome.clone();

            if shutdown.is_some() {
                continue;
            }

            if matches!(result.outcome, ServiceOutcome::Completed) {
                readiness.mark_ready(result.index);
            }

            if !readiness.is_ready(result.index) {
                shutdown = Some(ShutdownCause::ReadinessFailed {
                    service: state.registration.name,
                });
                root.cancel();
            }

            if shutdown.is_none() {
                match result.outcome {
                    ServiceOutcome::Completed => match state.registration.policy.on_completed() {
                        ExitAction::Ignore => {},
                        ExitAction::Restart => {
                            state.restarts += 1;
                            spawn_service(
                                &mut tasks,
                                &mut running,
                                &state.registration,
                                result.index,
                                root.clone(),
                                readiness.signal(result.index, state.registration.readiness),
                                None,
                            );
                            continue;
                        },
                        ExitAction::Shutdown => {
                            shutdown = Some(ShutdownCause::Requested);
                            root.cancel();
                        },
                    },
                    ServiceOutcome::Cancelled => {},
                    ServiceOutcome::RequestedShutdown => {
                        shutdown = Some(ShutdownCause::ServiceRequested {
                            service: state.registration.name,
                        });
                        root.cancel();
                    },
                    ServiceOutcome::Error(_) => {
                        match state.registration.policy.restart().action(state.restarts) {
                            FailureAction::Restart { backoff } => {
                                state.restarts += 1;
                                spawn_service(
                                    &mut tasks,
                                    &mut running,
                                    &state.registration,
                                    result.index,
                                    root.clone(),
                                    readiness.signal(result.index, state.registration.readiness),
                                    Some(backoff),
                                );
                                continue;
                            },
                            FailureAction::Terminal(ErrorAction::Ignore) => {},
                            FailureAction::Terminal(ErrorAction::Shutdown) => {
                                shutdown = Some(ShutdownCause::FatalService {
                                    service: state.registration.name,
                                });
                                root.cancel();
                            },
                        }
                    },
                }
            }

            if let Some(cause) = shutdown.take() {
                return finish_shutdown(shutdown_timeout, tasks, running, states, readiness, cause)
                    .await;
            }
        }
    }
}

/// Concrete registration stored by the builder and later consumed by
/// [`Supervisor::run`].
///
/// This is the internal bridge between the user-facing API and the runtime:
/// it freezes a service's stable name, resolved [`ServicePolicy`], startup
/// [`ReadinessMode`], and a type-erased runner that already owns the extracted
/// service context.
#[derive(Clone)]
struct Registration {
    name: &'static str,
    policy: ServicePolicy,
    readiness: ReadinessMode,
    runner: Arc<dyn Runner>,
}

impl Registration {
    fn new<S>(service: S, ctx: S::Context, policy: ServicePolicy, readiness: ReadinessMode) -> Self
    where
        S: SupervisedService,
    {
        Self {
            name: service.name(),
            policy,
            readiness,
            runner: Arc::new(ServiceRunner {
                service: Arc::new(service),
                ctx,
            }),
        }
    }

    fn run(
        &self,
        token: CancellationToken,
        readiness: ReadySignal,
        restart_delay: Option<Duration>,
    ) -> BoxFuture<ServiceOutcome> {
        self.runner.run(token, readiness, restart_delay)
    }
}

/// Type-erased execution hook used by [`Registration`].
///
/// The public API stays generic over [`SupervisedService`], but the runtime
/// needs a uniform way to spawn heterogeneous services after registration is
/// complete. [`Runner`] is that uniform internal boundary.
trait Runner: Send + Sync {
    fn run(
        &self,
        token: CancellationToken,
        readiness: ReadySignal,
        restart_delay: Option<Duration>,
    ) -> BoxFuture<ServiceOutcome>;
}

/// Concrete [`Runner`] for one [`SupervisedService`] and one extracted context.
///
/// This owns the cloned service value plus the context projected from the root
/// supervisor state at registration time.
struct ServiceRunner<S>
where
    S: SupervisedService,
{
    service: Arc<S>,
    ctx: S::Context,
}

impl<S> Runner for ServiceRunner<S>
where
    S: SupervisedService,
{
    fn run(
        &self,
        token: CancellationToken,
        readiness: ReadySignal,
        restart_delay: Option<Duration>,
    ) -> BoxFuture<ServiceOutcome> {
        let service = Arc::clone(&self.service);
        let ctx = self.ctx.clone();
        Box::pin(async move {
            if let Some(delay) = restart_delay {
                if token
                    .clone()
                    .run_until_cancelled_owned(sleep(delay))
                    .await
                    .is_none()
                {
                    return ServiceOutcome::Cancelled;
                }
            }

            service
                .run(Context::with_readiness(token, readiness, ctx))
                .await
        })
    }
}

/// Mutable runtime state tracked for one registered service while the
/// supervisor is running.
///
/// TODO: This is also the natural place to hang live service telemetry later,
/// for example Prometheus-facing counters, restart gauges, or last-outcome
/// timestamps.
struct ServiceState {
    registration: Registration,
    restarts: usize,
    last_outcome: ServiceOutcome,
}

impl ServiceState {
    fn new(registration: Registration) -> Self {
        Self {
            registration,
            restarts: 0,
            last_outcome: ServiceOutcome::Cancelled,
        }
    }
}

/// Result payload returned by spawned service tasks.
struct TaskResult {
    index: usize,
    outcome: ServiceOutcome,
}

fn spawn_service(
    tasks: &mut JoinSet<TaskResult>,
    running: &mut HashMap<TaskId, usize>,
    registration: &Registration,
    index: usize,
    root: CancellationToken,
    readiness: ReadySignal,
    restart_delay: Option<Duration>,
) {
    let future = registration.run(root, readiness, restart_delay);
    let handle = tasks.spawn(async move {
        TaskResult {
            index,
            outcome: future.await,
        }
    });
    running.insert(handle.id(), index);
}

/// Resolves a completed Tokio task back into one supervisor-tracked service.
fn join_result(
    joined: Result<(TaskId, TaskResult), JoinError>,
    running: &mut HashMap<TaskId, usize>,
    states: &mut [ServiceState],
) -> Result<TaskResult, Error> {
    match joined {
        Ok((task_id, result)) => {
            running.remove(&task_id).ok_or(Error::UnknownTask {
                task_id: task_id.to_string(),
            })?;
            Ok(result)
        },
        Err(source) if source.is_cancelled() => {
            let task_id = source.id();
            let index = running.remove(&task_id).ok_or(Error::UnknownTask {
                task_id: task_id.to_string(),
            })?;
            state_mut(states, index)?.last_outcome = ServiceOutcome::Cancelled;
            Ok(TaskResult::new(index, ServiceOutcome::Cancelled))
        },
        Err(source) => {
            let task_id = source.id();
            let index = running.remove(&task_id).ok_or(Error::UnknownTask {
                task_id: task_id.to_string(),
            })?;
            Ok(TaskResult::new(
                index,
                ServiceOutcome::failed(source.to_string()),
            ))
        },
    }
}

impl TaskResult {
    fn new(index: usize, outcome: ServiceOutcome) -> Self {
        Self { index, outcome }
    }
}

fn mark_cancelled(running: &HashMap<TaskId, usize>, states: &mut [ServiceState]) {
    for index in running.values().copied() {
        if let Some(state) = states.get_mut(index) {
            state.last_outcome = ServiceOutcome::Cancelled;
        }
    }
}

#[instrument(skip_all, fields(shutdown_cause = ?cause))]
async fn finish_shutdown(
    shutdown_timeout: Duration,
    mut tasks: JoinSet<TaskResult>,
    mut running: HashMap<TaskId, usize>,
    mut states: Vec<ServiceState>,
    readiness: ReadinessTracker,
    cause: ShutdownCause,
) -> Result<RunSummary, Error> {
    let started = Instant::now();

    while !tasks.is_empty() {
        let elapsed = started.elapsed();
        if elapsed >= shutdown_timeout {
            mark_cancelled(&running, &mut states);
            tasks.abort_all();
            break;
        }

        let remaining = shutdown_timeout - elapsed;
        match timeout(remaining, tasks.join_next_with_id()).await {
            Ok(Some(joined)) => {
                let result = join_result(joined, &mut running, &mut states)?;
                state_mut(&mut states, result.index)?.last_outcome = result.outcome;
            },
            Ok(None) => break,
            Err(_) => {
                mark_cancelled(&running, &mut states);
                tasks.abort_all();
                break;
            },
        }
    }

    while let Some(joined) = tasks.join_next_with_id().await {
        let result = join_result(joined, &mut running, &mut states)?;
        state_mut(&mut states, result.index)?.last_outcome = result.outcome;
    }

    Ok(RunSummary::new(
        cause,
        readiness.state(),
        summarize(&states),
    ))
}

fn summarize(states: &[ServiceState]) -> Vec<ServiceSummary> {
    states
        .iter()
        .map(|state| ServiceSummary {
            name: state.registration.name,
            outcome: state.last_outcome.clone(),
            restarts: state.restarts,
        })
        .collect()
}

fn state_mut(states: &mut [ServiceState], index: usize) -> Result<&mut ServiceState, Error> {
    states
        .get_mut(index)
        .ok_or(Error::UnknownServiceIndex { index })
}

#[derive(Debug, ThisError)]
pub enum Error {
    #[error("cannot run a supervisor without any registered services")]
    NoServices,
    #[error("received completion for unknown supervised task {task_id}")]
    UnknownTask { task_id: String },
    #[error("received an out-of-range service index {index} from supervisor bookkeeping")]
    UnknownServiceIndex { index: usize },
}
