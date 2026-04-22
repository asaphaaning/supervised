# Changelog

## 0.3.0 - 2026-04-22

### Added

- Added `IntoServiceOutcome` so `service_fn` closures can return `()`,
  `ServiceOutcome`, `Result<(), E>`, or `Result<ServiceOutcome, E>`.
- Added `IntoServiceError` so fallible `service_fn` closures can convert
  `ServiceError` or standard error types into supervisor error outcomes.
- Re-exported the new conversion traits from the crate root.
- Added coverage for unit-returning services, fallible service functions,
  explicit outcome preservation, direct `ServiceError` conversion, and startup
  readiness with `Result<(), E>`.

### Changed

- Updated README examples to use the more natural `service_fn` return shapes.
- Bumped the crate version to `0.3.0`.
