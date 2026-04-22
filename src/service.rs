//! Supervised service vocabulary and adapters.
use std::{convert::Infallible, error::Error, fmt, future::Future, pin::Pin, sync::Arc};

use crate::{readiness::ReadinessMode, Context};

/// Heap-allocated future returned by [`SupervisedService::run`].
pub type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send + 'static>>;

/// Terminal outcome reported by a supervised task.
///
/// This enum describes what happened inside the service.
/// The supervisor still decides what the system should do next through
/// [`ServicePolicy`](crate::ServicePolicy),
/// [`RestartPolicy`](crate::RestartPolicy),
/// and [`ExitAction`](crate::ExitAction).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ServiceOutcome {
    /// The service finished normally.
    Completed,
    /// The service observed supervisor shutdown and exited cooperatively.
    Cancelled,
    /// The service is asking the supervisor to begin global shutdown.
    RequestedShutdown,
    /// The service ended with a typed failure payload.
    Error(ServiceError),
}

impl ServiceOutcome {
    /// Convenience constructor for [`ServiceOutcome::Completed`].
    pub const fn completed() -> Self {
        Self::Completed
    }

    /// Convenience constructor for [`ServiceOutcome::Cancelled`].
    pub const fn cancelled() -> Self {
        Self::Cancelled
    }

    /// Convenience constructor for [`ServiceOutcome::RequestedShutdown`].
    pub const fn requested_shutdown() -> Self {
        Self::RequestedShutdown
    }

    /// Convenience constructor for [`ServiceOutcome::Error`].
    pub fn failed(error: impl Into<ServiceError>) -> Self {
        Self::Error(error.into())
    }
}

/// Stable, displayable failure payload carried by [`ServiceOutcome::Error`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServiceError {
    message: Arc<str>,
}

impl ServiceError {
    /// Builds a new service error from a message that should remain meaningful
    /// at supervisor boundaries and in summaries.
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: Arc::<str>::from(message.into()),
        }
    }

    /// Converts an external error into a stable [`ServiceError`] message.
    pub fn from_error(error: impl std::error::Error) -> Self {
        Self::new(error.to_string())
    }

    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for ServiceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl From<String> for ServiceError {
    fn from(message: String) -> Self {
        Self::new(message)
    }
}

impl From<&str> for ServiceError {
    fn from(message: &str) -> Self {
        Self::new(message)
    }
}

/// Converts fallible service errors into [`ServiceError`].
///
/// This trait keeps [`service_fn`] ergonomic for module-local error enums
/// while preserving [`ServiceOutcome`] as the supervisor runtime boundary.
pub trait IntoServiceError {
    /// Converts `self` into the stable service error payload used in summaries.
    fn into_service_error(self) -> ServiceError;
}

impl IntoServiceError for ServiceError {
    fn into_service_error(self) -> ServiceError {
        self
    }
}

impl<E> IntoServiceError for E
where
    E: Error + Send + Sync + 'static,
{
    fn into_service_error(self) -> ServiceError {
        ServiceError::from_error(self)
    }
}

/// Converts function-backed service return values into [`ServiceOutcome`].
///
/// Concrete [`SupervisedService`] implementations still return
/// [`ServiceOutcome`] directly. This trait is intentionally used by
/// [`service_fn`] so one-off async functions can use natural signatures like
/// `Result<(), Error>` without widening the core service trait.
pub trait IntoServiceOutcome {
    /// Converts `self` into the terminal outcome observed by the supervisor.
    fn into_service_outcome(self) -> ServiceOutcome;
}

impl IntoServiceOutcome for ServiceOutcome {
    fn into_service_outcome(self) -> ServiceOutcome {
        self
    }
}

impl IntoServiceOutcome for () {
    fn into_service_outcome(self) -> ServiceOutcome {
        ServiceOutcome::Completed
    }
}

impl IntoServiceOutcome for Infallible {
    fn into_service_outcome(self) -> ServiceOutcome {
        match self {}
    }
}

impl<T, E> IntoServiceOutcome for Result<T, E>
where
    T: IntoServiceOutcome,
    E: IntoServiceError,
{
    fn into_service_outcome(self) -> ServiceOutcome {
        match self {
            Ok(outcome) => outcome.into_service_outcome(),
            Err(error) => ServiceOutcome::Error(error.into_service_error()),
        }
    }
}

/// Long-lived async subsystem owned by the supervisor.
///
/// A [`SupervisedService`] gets a typed [`Context`] and returns a
/// [`ServiceOutcome`]. It does not decide restart or shutdown policy itself.
pub trait SupervisedService: Send + Sync + 'static {
    /// Typed payload injected alongside the supervisor cancellation token.
    type Context: Clone + Send + Sync + 'static;

    /// Stable identifier used in summaries and shutdown causes.
    fn name(&self) -> &'static str;

    /// Startup readiness behavior for this service.
    ///
    /// Most services are ready immediately. Use [`when_ready`] to opt a
    /// service into explicit startup readiness without widening the builder
    /// method surface.
    fn readiness(&self) -> ReadinessMode {
        ReadinessMode::Immediate
    }

    /// Runs the service until it reaches a terminal [`ServiceOutcome`].
    fn run(&self, ctx: Context<Self::Context>) -> BoxFuture<ServiceOutcome>;
}

/// Fluent decorators available on every [`SupervisedService`].
pub trait ServiceExt: SupervisedService + Sized {
    /// Wraps this service with [`when_ready`].
    fn when_ready(self) -> WhenReady<Self> {
        when_ready(self)
    }

    /// Wraps this service with [`until_cancelled`].
    fn until_cancelled(self) -> UntilCancelled<Self> {
        until_cancelled(self)
    }
}

impl<S> ServiceExt for S where S: SupervisedService {}

/// Function-backed [`SupervisedService`] returned by [`service_fn`].
///
/// `C` is inferred from the closure argument type, so call sites can describe
/// their required context without passing a separate context value.
#[derive(Clone)]
pub struct FnService<C, F> {
    name: &'static str,
    run: F,
    marker: std::marker::PhantomData<fn() -> C>,
}

impl<C, F> FnService<C, F> {
    pub fn name(&self) -> &'static str {
        self.name
    }
}

/// Wraps an async function as a [`SupervisedService`].
///
/// This keeps one-off services ergonomic without widening the core trait
/// surface or forcing callers into a monolithic service object pattern.
///
/// The resulting service can be registered with
/// [`SupervisorBuilder::add`](crate::SupervisorBuilder::add) or
/// [`SupervisorBuilder::add_with_options`](crate::SupervisorBuilder::add_with_options).
///
/// The closure's [`Context`] parameter determines the extracted service
/// context. With a stateful builder, that usually means:
/// - `Context<AppState>` when the service wants the full root state
/// - `Context<Subset>` when `Subset: FromSupervisorState<AppState>`
pub fn service_fn<C, F, Fut, O>(name: &'static str, run: F) -> FnService<C, F>
where
    C: Clone + Send + Sync + 'static,
    F: Fn(Context<C>) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = O> + Send + 'static,
    O: IntoServiceOutcome + Send + 'static,
{
    FnService {
        name,
        run,
        marker: std::marker::PhantomData,
    }
}

impl<C, F, Fut, O> SupervisedService for FnService<C, F>
where
    C: Clone + Send + Sync + 'static,
    F: Fn(Context<C>) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = O> + Send + 'static,
    O: IntoServiceOutcome + Send + 'static,
{
    type Context = C;

    fn name(&self) -> &'static str {
        self.name
    }

    fn run(&self, ctx: Context<Self::Context>) -> BoxFuture<ServiceOutcome> {
        let future = (self.run)(ctx);
        Box::pin(async move { future.await.into_service_outcome() })
    }
}

/// Wraps a service so supervisor startup readiness waits for it.
///
/// A startup-gated service becomes ready when it calls
/// [`ReadySignal::mark_ready`](crate::ReadySignal::mark_ready) or returns
/// [`ServiceOutcome::Completed`].
pub fn when_ready<S>(service: S) -> WhenReady<S>
where
    S: SupervisedService,
{
    WhenReady {
        service: Arc::new(service),
    }
}

/// [`SupervisedService`] adapter returned by [`when_ready`].
#[derive(Clone)]
pub struct WhenReady<S>
where
    S: SupervisedService,
{
    service: Arc<S>,
}

impl<S> SupervisedService for WhenReady<S>
where
    S: SupervisedService,
{
    type Context = S::Context;

    fn name(&self) -> &'static str {
        self.service.name()
    }

    fn readiness(&self) -> ReadinessMode {
        ReadinessMode::Explicit
    }

    fn run(&self, ctx: Context<Self::Context>) -> BoxFuture<ServiceOutcome> {
        self.service.run(ctx)
    }
}

/// Wraps a service so the supervisor cancellation token wins the outer race.
///
/// This is the convenient default for long-lived async loops that should stop
/// as soon as the supervisor begins shutdown.
pub fn until_cancelled<S>(service: S) -> UntilCancelled<S>
where
    S: SupervisedService,
{
    UntilCancelled {
        service: Arc::new(service),
    }
}

/// [`SupervisedService`] adapter returned by [`until_cancelled`].
#[derive(Clone)]
pub struct UntilCancelled<S>
where
    S: SupervisedService,
{
    service: Arc<S>,
}

impl<S> SupervisedService for UntilCancelled<S>
where
    S: SupervisedService,
{
    type Context = S::Context;

    fn name(&self) -> &'static str {
        self.service.name()
    }

    fn readiness(&self) -> ReadinessMode {
        self.service.readiness()
    }

    fn run(&self, ctx: Context<Self::Context>) -> BoxFuture<ServiceOutcome> {
        let token = ctx.token().clone();
        let service = Arc::clone(&self.service);
        Box::pin(async move {
            match token.run_until_cancelled_owned(service.run(ctx)).await {
                Some(outcome) => outcome,
                None => ServiceOutcome::Cancelled,
            }
        })
    }
}
