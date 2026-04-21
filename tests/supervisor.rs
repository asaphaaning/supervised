#![allow(clippy::expect_used)]

use std::{
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};

use supervised::{
    service_fn,
    BoxFuture,
    Context,
    ErrorAction,
    ExitAction,
    FromSupervisorState,
    Options,
    ReadinessMode,
    RestartPolicy,
    ServiceExt,
    ServiceOutcome,
    ServicePolicy,
    ShutdownCause,
    SupervisedService,
    SupervisorBuilder,
    SupervisorReadiness,
};
use tokio::{
    task::yield_now,
    time::{sleep, timeout},
};

struct ManualService;

impl SupervisedService for ManualService {
    type Context = String;

    fn name(&self) -> &'static str {
        "manual"
    }

    fn run(&self, ctx: Context<Self::Context>) -> BoxFuture<ServiceOutcome> {
        Box::pin(async move {
            if ctx.ctx() == "Hyprbaric" {
                ServiceOutcome::completed()
            } else {
                ServiceOutcome::failed("unexpected manual service context")
            }
        })
    }
}

struct ManualReadyService;

impl SupervisedService for ManualReadyService {
    type Context = String;

    fn name(&self) -> &'static str {
        "manual-ready"
    }

    fn run(&self, ctx: Context<Self::Context>) -> BoxFuture<ServiceOutcome> {
        Box::pin(async move {
            if ctx.ctx() == "Hyprbaric" {
                ctx.readiness().mark_ready();
                ServiceOutcome::completed()
            } else {
                ServiceOutcome::failed("unexpected manual service context")
            }
        })
    }
}

#[tokio::test(flavor = "current_thread")]
async fn completes_without_shutdown_request() {
    let summary = SupervisorBuilder::new(())
        .add(service_fn("complete", |_ctx: Context<()>| async move {
            ServiceOutcome::completed()
        }))
        .build()
        .run()
        .await
        .expect("supervisor should run");

    assert_eq!(summary.shutdown_cause(), &ShutdownCause::Completed);
    assert_eq!(summary.readiness(), SupervisorReadiness::Ready);
    assert_eq!(
        summary
            .service("complete")
            .expect("service summary")
            .outcome(),
        &ServiceOutcome::Completed
    );
}

#[tokio::test(flavor = "current_thread")]
async fn requested_shutdown_cancels_other_services() {
    let summary = SupervisorBuilder::new(())
        .add(service_fn("watcher", |ctx: Context<()>| async move {
            ctx.token().cancelled().await;
            ServiceOutcome::cancelled()
        }))
        .add(service_fn("shutdown", |_ctx: Context<()>| async move {
            ServiceOutcome::requested_shutdown()
        }))
        .build()
        .run()
        .await
        .expect("supervisor should run");

    assert_eq!(
        summary.shutdown_cause(),
        &ShutdownCause::ServiceRequested {
            service: "shutdown"
        }
    );
    assert_eq!(
        summary
            .service("watcher")
            .expect("watcher summary")
            .outcome(),
        &ServiceOutcome::Cancelled
    );
}

#[tokio::test(flavor = "current_thread")]
async fn restart_policy_retries_until_service_completes() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let service_attempts = Arc::clone(&attempts);

    let summary = SupervisorBuilder::new(())
        .add_with_options(
            service_fn("flaky", move |_ctx: Context<()>| {
                let attempts = Arc::clone(&service_attempts);
                async move {
                    let attempt = attempts.fetch_add(1, Ordering::SeqCst);
                    if attempt < 2 {
                        ServiceOutcome::failed(format!("attempt {attempt} failed"))
                    } else {
                        ServiceOutcome::completed()
                    }
                }
            }),
            Options::new().policy(ServicePolicy::new(
                ExitAction::Ignore,
                RestartPolicy::attempts(3, Duration::from_millis(1), ErrorAction::Shutdown),
            )),
        )
        .build()
        .run()
        .await
        .expect("supervisor should run");

    assert_eq!(summary.shutdown_cause(), &ShutdownCause::Completed);
    let flaky = summary.service("flaky").expect("flaky summary");
    assert_eq!(flaky.outcome(), &ServiceOutcome::Completed);
    assert_eq!(flaky.restarts(), 2);
    assert_eq!(attempts.load(Ordering::SeqCst), 3);
}

#[tokio::test(flavor = "current_thread")]
async fn exhausted_restart_policy_triggers_shutdown() {
    let summary = SupervisorBuilder::new(())
        .add(service_fn("observer", |ctx: Context<()>| async move {
            ctx.token().cancelled().await;
            ServiceOutcome::cancelled()
        }))
        .add_with_options(
            service_fn("fatal", |_ctx: Context<()>| async move {
                ServiceOutcome::failed("boom")
            }),
            Options::new().policy(ServicePolicy::new(
                ExitAction::Ignore,
                RestartPolicy::attempts(1, Duration::from_millis(1), ErrorAction::Shutdown),
            )),
        )
        .build()
        .run()
        .await
        .expect("supervisor should run");

    assert_eq!(
        summary.shutdown_cause(),
        &ShutdownCause::FatalService { service: "fatal" }
    );
    assert_eq!(
        summary.service("fatal").expect("fatal summary").restarts(),
        1
    );
    assert_eq!(
        summary
            .service("observer")
            .expect("observer summary")
            .outcome(),
        &ServiceOutcome::Cancelled
    );
}

#[tokio::test(flavor = "current_thread")]
async fn shutdown_timeout_aborts_lingering_tasks() {
    let summary = SupervisorBuilder::new(())
        .shutdown_timeout(Duration::from_millis(10))
        .add(service_fn("shutdown", |_ctx: Context<()>| async move {
            ServiceOutcome::requested_shutdown()
        }))
        .add(service_fn("linger", |_ctx: Context<()>| async move {
            sleep(Duration::from_secs(60)).await;
            ServiceOutcome::completed()
        }))
        .build()
        .run()
        .await
        .expect("supervisor should run");

    assert_eq!(
        summary.shutdown_cause(),
        &ShutdownCause::ServiceRequested {
            service: "shutdown"
        }
    );
    assert_eq!(
        summary.service("linger").expect("linger summary").outcome(),
        &ServiceOutcome::Cancelled
    );
}

#[tokio::test(flavor = "current_thread")]
async fn service_fn_receives_typed_context() {
    let summary = SupervisorBuilder::new(String::from("Hyprbaric"))
        .add(service_fn("typed", |ctx: Context<String>| async move {
            if ctx.ctx() == "Hyprbaric" {
                ServiceOutcome::completed()
            } else {
                ServiceOutcome::failed("unexpected context")
            }
        }))
        .build()
        .run()
        .await
        .expect("supervisor should run");

    assert_eq!(
        summary.service("typed").expect("typed summary").outcome(),
        &ServiceOutcome::Completed
    );
}

#[test]
fn never_policy_never_restarts() {
    let policy = RestartPolicy::never(ErrorAction::Shutdown);

    assert_eq!(policy.backoff(), None);
    assert_eq!(policy.max_restarts(), None);
    assert_eq!(policy.on_error(), Some(ErrorAction::Shutdown));
}

#[derive(Clone)]
struct AppState {
    label: String,
}

#[derive(Clone)]
struct LabelContext {
    label: String,
}

impl FromSupervisorState<AppState> for LabelContext {
    fn from_state(state: &AppState) -> Self {
        Self {
            label: state.label.clone(),
        }
    }
}

#[tokio::test(flavor = "current_thread")]
async fn builder_extracts_service_context_from_root_state() {
    let summary = SupervisorBuilder::new(AppState {
        label: String::from("Hyprbaric"),
    })
    .add(service_fn(
        "subset",
        |ctx: Context<LabelContext>| async move {
            if ctx.ctx().label == "Hyprbaric" {
                ServiceOutcome::completed()
            } else {
                ServiceOutcome::failed("unexpected label")
            }
        },
    ))
    .build()
    .run()
    .await
    .expect("supervisor should run");

    assert_eq!(
        summary.service("subset").expect("subset summary").outcome(),
        &ServiceOutcome::Completed
    );
}

#[tokio::test(flavor = "current_thread")]
async fn until_cancelled_wraps_long_lived_functions() {
    let summary = SupervisorBuilder::new(())
        .add(
            service_fn("loop", |_ctx: Context<()>| async move {
                loop {
                    sleep(Duration::from_secs(60)).await;
                }
                #[allow(unreachable_code)]
                ServiceOutcome::completed()
            })
            .until_cancelled(),
        )
        .add(service_fn("shutdown", |_ctx: Context<()>| async move {
            ServiceOutcome::requested_shutdown()
        }))
        .build()
        .run()
        .await
        .expect("supervisor should run");

    assert_eq!(
        summary.service("loop").expect("loop summary").outcome(),
        &ServiceOutcome::Cancelled
    );
}

#[tokio::test(flavor = "current_thread")]
async fn manual_service_registration_path_works() {
    let summary = SupervisorBuilder::new(String::from("Hyprbaric"))
        .add(ManualService)
        .build()
        .run()
        .await
        .expect("supervisor should run");

    assert_eq!(summary.shutdown_cause(), &ShutdownCause::Completed);
    assert_eq!(
        summary.service("manual").expect("manual summary").outcome(),
        &ServiceOutcome::Completed
    );
}

#[tokio::test(flavor = "current_thread")]
async fn explicit_readiness_starts_pending_and_becomes_ready() {
    let supervisor = SupervisorBuilder::new(())
        .add(
            service_fn("gate", |ctx: Context<()>| async move {
                ctx.readiness().mark_ready();
                ServiceOutcome::requested_shutdown()
            })
            .when_ready(),
        )
        .build();
    let readiness = supervisor.readiness();

    assert_eq!(*readiness.borrow(), SupervisorReadiness::Pending);

    let summary = supervisor.run().await.expect("supervisor should run");

    assert_eq!(
        summary.shutdown_cause(),
        &ShutdownCause::ServiceRequested { service: "gate" }
    );
    assert_eq!(summary.readiness(), SupervisorReadiness::Ready);
    assert_eq!(*readiness.borrow(), SupervisorReadiness::Ready);
}

#[tokio::test(flavor = "current_thread")]
async fn startup_gated_error_before_ready_triggers_shutdown() {
    let summary = SupervisorBuilder::new(())
        .add(service_fn("observer", |ctx: Context<()>| async move {
            ctx.token().cancelled().await;
            ServiceOutcome::cancelled()
        }))
        .add(
            service_fn("gate", |_ctx: Context<()>| async move {
                ServiceOutcome::failed("startup failed")
            })
            .when_ready(),
        )
        .build()
        .run()
        .await
        .expect("supervisor should run");

    assert_eq!(
        summary.shutdown_cause(),
        &ShutdownCause::ReadinessFailed { service: "gate" }
    );
    assert_eq!(summary.readiness(), SupervisorReadiness::Pending);
    assert_eq!(
        summary
            .service("observer")
            .expect("observer summary")
            .outcome(),
        &ServiceOutcome::Cancelled
    );
}

#[tokio::test(flavor = "current_thread")]
async fn startup_gated_requested_shutdown_before_ready_triggers_readiness_failure() {
    let summary = SupervisorBuilder::new(())
        .add(
            service_fn("gate", |_ctx: Context<()>| async move {
                ServiceOutcome::requested_shutdown()
            })
            .when_ready(),
        )
        .build()
        .run()
        .await
        .expect("supervisor should run");

    assert_eq!(
        summary.shutdown_cause(),
        &ShutdownCause::ReadinessFailed { service: "gate" }
    );
    assert_eq!(summary.readiness(), SupervisorReadiness::Pending);
}

#[tokio::test(flavor = "current_thread")]
async fn startup_gated_cancelled_before_ready_triggers_readiness_failure() {
    let summary = SupervisorBuilder::new(())
        .add(
            service_fn("gate", |_ctx: Context<()>| async move {
                ServiceOutcome::cancelled()
            })
            .when_ready(),
        )
        .build()
        .run()
        .await
        .expect("supervisor should run");

    assert_eq!(
        summary.shutdown_cause(),
        &ShutdownCause::ReadinessFailed { service: "gate" }
    );
    assert_eq!(summary.readiness(), SupervisorReadiness::Pending);
}

#[tokio::test(flavor = "current_thread")]
async fn startup_gated_completion_counts_as_ready() {
    let summary = SupervisorBuilder::new(())
        .add(
            service_fn("gate", |_ctx: Context<()>| async move {
                ServiceOutcome::completed()
            })
            .when_ready(),
        )
        .build()
        .run()
        .await
        .expect("supervisor should run");

    assert_eq!(summary.shutdown_cause(), &ShutdownCause::Completed);
    assert_eq!(summary.readiness(), SupervisorReadiness::Ready);
    assert_eq!(
        summary.service("gate").expect("gate summary").outcome(),
        &ServiceOutcome::Completed
    );
}

#[tokio::test(flavor = "current_thread")]
async fn readiness_waits_for_every_startup_gate() {
    let supervisor = SupervisorBuilder::new(())
        .add(
            service_fn("first", |ctx: Context<()>| async move {
                ctx.readiness().mark_ready();
                ctx.token().cancelled().await;
                ServiceOutcome::cancelled()
            })
            .when_ready(),
        )
        .add(
            service_fn("second", |ctx: Context<()>| async move {
                yield_now().await;
                ctx.readiness().mark_ready();
                ServiceOutcome::requested_shutdown()
            })
            .when_ready(),
        )
        .build();
    let mut readiness = supervisor.readiness();

    assert_eq!(*readiness.borrow(), SupervisorReadiness::Pending);

    let handle = tokio::spawn(supervisor.run());
    assert!(timeout(Duration::from_millis(50), readiness.changed())
        .await
        .expect("readiness should change")
        .is_ok());

    assert_eq!(*readiness.borrow(), SupervisorReadiness::Ready);

    let summary = handle
        .await
        .expect("supervisor task should join")
        .expect("supervisor should run");

    assert_eq!(
        summary.shutdown_cause(),
        &ShutdownCause::ServiceRequested { service: "second" }
    );
    assert_eq!(summary.readiness(), SupervisorReadiness::Ready);
}

#[tokio::test(flavor = "current_thread")]
async fn mark_ready_is_idempotent() {
    let summary = SupervisorBuilder::new(())
        .add(
            service_fn("gate", |ctx: Context<()>| async move {
                ctx.readiness().mark_ready();
                ctx.readiness().mark_ready();
                ServiceOutcome::requested_shutdown()
            })
            .when_ready(),
        )
        .build()
        .run()
        .await
        .expect("supervisor should run");

    assert_eq!(summary.readiness(), SupervisorReadiness::Ready);
    assert_eq!(
        summary.shutdown_cause(),
        &ShutdownCause::ServiceRequested { service: "gate" }
    );
}

#[tokio::test(flavor = "current_thread")]
async fn readiness_failure_ignores_restart_policy() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let service_attempts = Arc::clone(&attempts);

    let summary = SupervisorBuilder::new(())
        .add_with_options(
            service_fn("gate", move |_ctx: Context<()>| {
                let attempts = Arc::clone(&service_attempts);
                async move {
                    attempts.fetch_add(1, Ordering::SeqCst);
                    ServiceOutcome::failed("startup failed")
                }
            })
            .when_ready(),
            Options::new().policy(ServicePolicy::new(
                ExitAction::Ignore,
                RestartPolicy::always(Duration::from_millis(1)),
            )),
        )
        .build()
        .run()
        .await
        .expect("supervisor should run");

    assert_eq!(
        summary.shutdown_cause(),
        &ShutdownCause::ReadinessFailed { service: "gate" }
    );
    assert_eq!(summary.service("gate").expect("gate summary").restarts(), 0);
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
}

#[tokio::test(flavor = "current_thread")]
async fn explicit_policy_registration_can_be_startup_gated() {
    let summary = SupervisorBuilder::new(())
        .add_with_options(
            service_fn("loop", |ctx: Context<()>| async move {
                ctx.readiness().mark_ready();
                ctx.token().cancelled().await;
                ServiceOutcome::cancelled()
            })
            .when_ready()
            .until_cancelled(),
            Options::new().policy(ServicePolicy::new(
                ExitAction::Ignore,
                RestartPolicy::never(ErrorAction::Ignore),
            )),
        )
        .add(service_fn("shutdown", |_ctx: Context<()>| async move {
            ServiceOutcome::requested_shutdown()
        }))
        .build()
        .run()
        .await
        .expect("supervisor should run");

    assert_eq!(summary.readiness(), SupervisorReadiness::Ready);
    assert_eq!(
        summary.service("loop").expect("loop summary").outcome(),
        &ServiceOutcome::Cancelled
    );
}

#[tokio::test(flavor = "current_thread")]
async fn readiness_stays_ready_after_ready_service_restarts() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let service_attempts = Arc::clone(&attempts);

    let summary = SupervisorBuilder::new(())
        .add_with_options(
            service_fn("flaky", move |ctx: Context<()>| {
                let attempts = Arc::clone(&service_attempts);
                async move {
                    ctx.readiness().mark_ready();
                    let attempt = attempts.fetch_add(1, Ordering::SeqCst);
                    if attempt == 0 {
                        ServiceOutcome::failed("after-ready failure")
                    } else {
                        ServiceOutcome::completed()
                    }
                }
            })
            .when_ready(),
            Options::new().policy(ServicePolicy::new(
                ExitAction::Ignore,
                RestartPolicy::attempts(1, Duration::from_millis(1), ErrorAction::Shutdown),
            )),
        )
        .build()
        .run()
        .await
        .expect("supervisor should run");

    assert_eq!(summary.shutdown_cause(), &ShutdownCause::Completed);
    assert_eq!(summary.readiness(), SupervisorReadiness::Ready);
    assert_eq!(
        summary.service("flaky").expect("flaky summary").restarts(),
        1
    );
}

#[tokio::test(flavor = "current_thread")]
async fn until_cancelled_service_can_be_startup_gated() {
    let summary = SupervisorBuilder::new(())
        .add(
            service_fn("loop", |ctx: Context<()>| async move {
                ctx.readiness().mark_ready();
                loop {
                    sleep(Duration::from_secs(60)).await;
                }
                #[allow(unreachable_code)]
                ServiceOutcome::completed()
            })
            .when_ready()
            .until_cancelled(),
        )
        .add(service_fn("shutdown", |_ctx: Context<()>| async move {
            ServiceOutcome::requested_shutdown()
        }))
        .build()
        .run()
        .await
        .expect("supervisor should run");

    assert_eq!(summary.readiness(), SupervisorReadiness::Ready);
    assert_eq!(
        summary.service("loop").expect("loop summary").outcome(),
        &ServiceOutcome::Cancelled
    );
}

#[tokio::test(flavor = "current_thread")]
async fn manual_service_can_use_readiness_signal() {
    let summary = SupervisorBuilder::new(String::from("Hyprbaric"))
        .add(ManualReadyService.when_ready())
        .build()
        .run()
        .await
        .expect("supervisor should run");

    assert_eq!(summary.shutdown_cause(), &ShutdownCause::Completed);
    assert_eq!(summary.readiness(), SupervisorReadiness::Ready);
    assert_eq!(
        summary
            .service("manual-ready")
            .expect("manual ready summary")
            .outcome(),
        &ServiceOutcome::Completed
    );
}

#[tokio::test(flavor = "current_thread")]
async fn options_can_override_service_readiness() {
    let summary = SupervisorBuilder::new(())
        .add_with_options(
            service_fn("gate", |_ctx: Context<()>| async move {
                ServiceOutcome::requested_shutdown()
            }),
            Options::new().readiness(ReadinessMode::Explicit),
        )
        .build()
        .run()
        .await
        .expect("supervisor should run");

    assert_eq!(
        summary.shutdown_cause(),
        &ShutdownCause::ReadinessFailed { service: "gate" }
    );
    assert_eq!(summary.readiness(), SupervisorReadiness::Pending);
}
