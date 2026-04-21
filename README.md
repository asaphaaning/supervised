# supervised

`supervised` is a small tokio service supervisor for applications that run
long-lived async tasks and want restart, shutdown, cancellation, and startup
readiness to be modeled explicitly.

The crate is intentionally compact:

- `SupervisedService` is the single service trait.
- `service_fn` wraps one-off async functions as services.
- `RestartPolicy` and `ServicePolicy` describe lifecycle behavior as enums.
- `.until_cancelled()` and `.when_ready()` are fluent service adapters.
- `.shutdown_on_ctrl_c()` adds an immediately-ready Ctrl+C shutdown listener.
- `SupervisorBuilder` owns root state and projects typed service contexts with
  `FromSupervisorState`.

## Example

```rust
use supervised::{
    Context, ServiceExt, ServiceOutcome, SupervisorBuilder, service_fn,
};

#[derive(Clone)]
struct App {
    name: &'static str,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), supervised::Error> {
    let summary = SupervisorBuilder::new(App { name: "bar" })
        .shutdown_on_ctrl_c()
        .add(
            service_fn("worker", |ctx: Context<App>| async move {
                tracing::info!(app = ctx.ctx().name, "worker started");

                std::future::pending::<ServiceOutcome>().await
            })
            .until_cancelled(),
        )
        .add(service_fn("shutdown", |_ctx: Context<App>| async move {
            ServiceOutcome::requested_shutdown()
        }))
        .build()
        .run()
        .await?;

    tracing::info!(cause = ?summary.shutdown_cause(), "supervisor stopped");

    Ok(())
}
```

## External cancellation

Brought your own `CancellationToken`? No problem. Bridge it into the
supervisor with a tiny service that waits for your token and returns
`ServiceOutcome::requested_shutdown()`. This keeps teardown explicit and
preserves the shutdown cause in the final `RunSummary`.

```rust
use supervised::{Context, ServiceOutcome, SupervisorBuilder, service_fn};
use tokio_util::sync::CancellationToken;

async fn run(external_token: CancellationToken) -> Result<(), supervised::Error> {
let supervisor = SupervisorBuilder::new(())
    .add(service_fn("shutdown", move |_ctx: Context<()>| {
        let token = external_token.clone();

        async move {
            token.cancelled().await;
            ServiceOutcome::requested_shutdown()
        }
    }))
    .build();

let _summary = supervisor.run().await?;

Ok(())
}
```

## Readiness

Services are ready immediately by default. Use `.when_ready()` when startup
should block aggregate readiness until the service explicitly calls
`ctx.readiness().mark_ready()` or completes successfully.

```rust
use supervised::{Context, ServiceExt, ServiceOutcome, SupervisorBuilder, service_fn};

async fn run() -> Result<(), supervised::Error> {
let supervisor = SupervisorBuilder::new(())
    .add(
        service_fn("cache", |ctx: Context<()>| async move {
            // Warm the cache here.
            ctx.readiness().mark_ready();
            ServiceOutcome::completed()
        })
        .when_ready(),
    )
    .build();

let _summary = supervisor.run().await?;

Ok(())
}
```

## License

Licensed under either of Apache License, Version 2.0 or MIT license at your
option.
