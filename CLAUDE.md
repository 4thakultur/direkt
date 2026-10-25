# Direkt — Claude Code Project Guidelines

## 1. Project Overview

Direkt is a self-hostable direct download manager written in Rust.

Its goal is to take the best features of JDownloader and improve upon them, with a particular focus on homelab users who want a reliable, automated, lightweight, and easy-to-maintain download service.

Direkt runs as a long-lived service on a home server, NAS, mini-PC, or other self-hosted infrastructure. Users can submit download URLs from their local network or remotely, monitor downloads through a web interface or API, and automatically organize downloaded media into libraries compatible with Plex and Jellyfin.

The intended workflow is simple:

1. A user submits a URL.
2. Direkt validates the request and determines which download provider can handle it.
3. The download enters a persistent queue and runs according to configured limits.
4. Direkt tracks progress, retries recoverable failures, and persists state.
5. When the transfer succeeds, Direkt identifies the media when applicable and organizes it into the configured destination.
6. Direkt reports completion or any failure requiring attention in the web UI and API. (Push notifications come after v1; see section 3.1.)

The system should operate unattended wherever possible. Users should not need to manually copy, rename, or move files during normal operation.

### Product principles

- **Self-hosted first:** Users control their data, files, credentials, and configuration.
- **Automation first:** Minimize manual intervention from URL submission to final destination.
- **Reliability first:** Downloads and their state must survive service restarts and recover gracefully from failures.
- **Simple deployment:** Docker Compose should provide a straightforward, documented installation path.
- **Modular by design:** Download providers, notifications, and media identification must be replaceable without coupling unrelated components.
- **Secure by default:** Network exposure, untrusted URLs, provider credentials, and filesystem operations must be handled safely.
- **Resource-efficient:** Avoid unnecessary CPU, memory, disk, and network usage.
- **Operationally transparent:** Provide meaningful progress, logs, error messages, and health information.
- **Incremental development:** Build a reliable MVP before expanding into advanced features.

## 2. MVP Scope

The MVP must deliver the following capabilities.

### 2.1 Web interface and API

Provide a web UI and a documented API from the beginning.

The web UI should allow users to:

- Submit a download URL.
- View queued, active, completed, failed, and post-processing downloads.
- Inspect progress, file size, transfer speed, and errors when available.
- Pause, resume, retry, and cancel downloads where supported.
- Inspect download details and relevant event history.
- Configure essential application settings.
- Configure media destinations and organization behavior.

The API should expose the core operations required by the UI and future external integrations.

Requirements:

- Use consistent request and response formats.
- Validate inputs and return actionable errors.
- Represent download states consistently across the API, UI, and persistence layer.
- Document endpoints, authentication requirements, and error responses.
- Provide an API specification, preferably OpenAPI, if compatible with the chosen framework.
- Avoid duplicating business logic between API handlers and other application components.

The API and UI must use the same underlying application services.

Do not introduce a separate frontend deployment or microservice architecture unless a concrete requirement justifies it.

### 2.2 Download engine

Support ordinary HTTP and HTTPS downloads in the MVP.

The engine must:

- Accept and validate download requests.
- Maintain a persistent queue.
- Track download state and progress.
- Support configurable download concurrency.
- Support cancellation and pausing where technically feasible.
- Support resumption when the server and remote resource permit it.
- Retry transient failures using bounded retries and backoff.
- Recover interrupted downloads after a service restart.
- Preserve partial downloads when doing so is safe and useful.
- Detect insufficient disk space and handle write failures.
- Prevent concurrent workers from processing the same job unintentionally.

Do not assume every server supports byte ranges, reliable content lengths, or resumable downloads. Verify the relevant capabilities before attempting to resume a transfer.

Distinguish between a completed transfer and a fully completed workflow. A transfer can succeed while subsequent media organization remains pending or fails.

### 2.3 Provider architecture

Create a provider abstraction from the beginning, even though the initial implementation remains small.

The initial HTTP/HTTPS provider must handle standard direct download URLs. The architecture must permit additional file-hosting integrations, starting with 1fichier.

The provider layer should encapsulate host-specific behavior such as:

- Recognizing supported URLs.
- Resolving a submitted URL into a downloadable resource.
- Handling authentication when required.
- Interpreting provider-specific errors and restrictions.
- Reporting relevant capabilities, including resumption where applicable.
- Applying provider-specific limits and retry guidance where necessary.

Keep provider-specific behavior out of the core queue and download lifecycle logic.

A provider must be able to indicate that a URL is unsupported or requires user intervention.

Do not implement fake or incomplete provider support merely to make an interface appear extensible. Only advertise capabilities that actually work.

Do not build a dynamic third-party plugin marketplace or a complex plugin-loading framework for the MVP. A small, well-defined Rust interface is sufficient.

### 2.4 Notifications (deferred)

Telegram notifications have been moved to after v1 (see section 3.1). The MVP reports download state, failures, and items needing attention through the web UI and API only.

### 2.5 Media identification and organization

Automatically identify movies and television episodes after a successful transfer.

Use filename parsing as the initial identification mechanism, supplemented by metadata lookup to improve accuracy.

The system should support:

- Distinguishing movies from television episodes when sufficient information is available.
- Extracting titles, release years, season numbers, and episode numbers where possible.
- Using a metadata provider to resolve ambiguous titles when configured.
- Configurable movie and television library destinations.
- Configurable naming and directory conventions.
- Safe movement of completed files into the appropriate destination.
- A configurable fallback location for unidentified or ambiguous media.
- Clear reporting when identification or organization fails.

Prefer a replaceable metadata-provider interface rather than tightly coupling the application to a single external service.

Metadata lookup should be optional or gracefully degradable. A metadata service outage must not prevent an otherwise successful download.

Requirements for filesystem operations:

- Never process a file before its transfer is complete.
- Prevent path traversal and unsafe destination paths.
- Avoid silently overwriting existing files.
- Handle cross-filesystem moves correctly.
- Make interrupted organization operations recoverable.
- Preserve completed downloads when identification or movement fails.
- Record the outcome of post-processing separately from the transfer result.

Use deterministic filename parsing and conservative matching. Do not silently place an ambiguous file into a potentially incorrect library directory.

The goal is compatibility with Plex and Jellyfin through correctly structured directories and filenames, not a dependency on either media server.

### 2.6 Persistence and recovery

The download queue must be persistent.

Select a suitable persistence technology after inspecting the repository and its existing dependencies. For a single-instance homelab service, prefer a solution that is simple to operate, reliable, and easy to back up.

Persist at least:

- Download identifiers and submitted URLs, subject to credential and privacy considerations.
- Current download states.
- Relevant progress and retry information.
- Destination paths and provider information.
- Error information needed for recovery and troubleshooting.
- Post-processing state.
- Essential application configuration and event history as appropriate.

On startup, reconcile persisted state with files on disk and the actual status of interrupted jobs.

Do not mark an interrupted download as completed simply because a partial file exists.

Avoid introducing a distributed database, message broker, or external queue service unless the requirements demonstrably demand it.

## 3. Explicitly Out of Scope for the MVP

Keep the following features separate from MVP implementation requirements:

- Notifications of any kind. Telegram is the first planned channel after v1 (section 3.1); other channels come later still.
- Browser extensions, bookmarklets, and mobile applications.
- Sonarr, Radarr, and other media automation integrations.
- Password-protected archive management. (Plain archives of movies and series are extracted; see `docs/media-organization.md`.)
- Advanced scheduling and bandwidth shaping.
- Permission systems beyond the two roles (administrator, user) and the fixed set of per-user permissions, which the project owner scheduled and which are implemented (see `docs/security.md`): no groups, custom roles, quotas, or self-registration.
- A dynamic third-party plugin marketplace.
- Distributed workers or multi-instance coordination.
- Automatic cleanup policies that delete completed downloads. (Exception, scheduled by the project owner: the opt-in "delete archives after extraction" setting, off by default.)
- Advanced duplicate detection across different providers.
- Sophisticated media matching beyond filename parsing and optional metadata lookup.
- Built-in VPN management or proxy orchestration.

Design clean interfaces where appropriate, but do not implement future features speculatively.

A future feature must not compromise the reliability, maintainability, or simplicity of the MVP.

### 3.1 Planned after v1: Telegram notifications

Not part of v1. Do not implement it until the project owner schedules it. When it is scheduled, these requirements apply.

Implement Telegram as the first notification channel.

Support notifications for:

- Successful download completion.
- Permanent download failure.
- A blocked download or one requiring user intervention.
- Low disk space.
- Media identification or organization failure when actionable.

Notifications should be generated from application events rather than sent directly from arbitrary parts of the download engine.

Requirements:

- Configure the Telegram bot token and destination through secure configuration.
- Never log bot tokens or expose them through the API.
- Prevent notification failures from changing a successful download into a failed download.
- Avoid duplicate notifications caused by ordinary state updates or restart recovery.
- Include enough context to identify the affected download without unnecessarily exposing sensitive URLs.
- Make notification behavior configurable.

Keep the notification interface independent of Telegram so future channels can be added without changing the download lifecycle.

Additional notification channels are out of scope even then.

## 4. Architecture and Separation of Concerns

Organize the application around clear responsibilities.

The following are logical components, not a mandate to create separate crates for every item:

- **API layer:** HTTP endpoints, request validation, response serialization, and API documentation.
- **Web UI:** User-facing download management and essential configuration.
- **Application services:** Business operations and coordination between components.
- **Download engine:** Queue processing, concurrency, transfers, retries, and recovery.
- **Provider layer:** URL detection, resolution, authentication, and provider-specific behavior.
- **Persistence layer:** Durable application state and migrations.
- **Notification layer (after v1):** Event-driven notification delivery.
- **Media processing layer:** Filename parsing, metadata lookup, and filesystem organization.
- **Observability:** Logging, health checks, and operational diagnostics.

Keep business rules out of HTTP handlers, UI components, and persistence-specific code.

Avoid circular dependencies and avoid having unrelated components modify download state independently.

Prefer the smallest architecture that provides the required boundaries.

### 4.1 Application services

Expose application operations through reusable service interfaces or modules.

For example, submitting a download should use the same validation and queueing logic regardless of whether the request comes from the web UI, API, or a future integration.

Do not implement separate business logic for each entry point.

### 4.2 Asynchronous execution

Network transfers and other long-running operations must not block the API server.

Use bounded concurrency and ensure background work can be cancelled or shut down safely.

Do not spawn unbounded tasks for submitted downloads or notification events.

Avoid holding synchronization locks across slow operations or `.await` points unless the design explicitly requires it.

### 4.3 Download state machine

Define an explicit download state model and validate state transitions.

At minimum, distinguish:

- Queued.
- Downloading.
- Paused.
- Completed.
- Failed.

Add blocked, retrying, cancelled, or post-processing states when justified by the actual lifecycle.

Do not conflate a temporary provider restriction with a permanent failure.

Represent post-processing separately from transfer completion. A successful transfer must remain recorded as successful even when media organization fails.

Document the allowed transitions and implement them consistently across persistence, API responses, and the UI.

### 4.4 Error model

Represent errors in a structured way.

Where applicable, distinguish:

- Invalid request or URL.
- Unsupported provider.
- Authentication failure.
- Temporary provider restriction.
- Network failure.
- Remote resource unavailable.
- Insufficient disk space.
- Local filesystem failure.
- Persistence failure.
- Media identification failure.
- Notification delivery failure.

Expose safe, actionable error descriptions to users. Keep internal diagnostic context in structured logs without exposing secrets.

## 5. Configuration and Deployment

### 5.1 Docker-first deployment

Docker and Docker Compose are the primary deployment targets.

The repository should provide:

- A Dockerfile suitable for reproducible builds.
- A Docker Compose example.
- Documented persistent volumes and bind mounts.
- Documented environment variables or a clear configuration file.
- Health checks where practical.
- A documented upgrade procedure.
- Instructions for backup and recovery.
- A clear explanation of filesystem permissions and ownership.

Prefer a small runtime image and a reproducible build process. Do not sacrifice security or debuggability for minimal image size.

Do not assume container paths are identical to host paths. Configure download, temporary, and media-library directories explicitly.

Document any shared-volume requirements for moving files into media libraries.

### 5.2 Configuration

Provide documented configuration for:

- HTTP listening address and port.
- Authentication and access control.
- Download storage.
- Temporary and partial-download storage.
- Download concurrency and retry behavior.
- Provider credentials.
- Movie and television library destinations.
- Metadata lookup settings.
- Disk-space warning thresholds.
- Logging and retention.

Validate configuration and report invalid values clearly.

Use safe defaults and never overwrite existing user configuration during upgrades.

Keep secrets out of source control, example configuration files, logs, and API responses.

Avoid creating multiple competing configuration mechanisms without a clear reason.

## 6. Security Requirements

Direkt handles untrusted URLs and may be exposed to the internet. Security is mandatory even in a single-user homelab deployment.

### Network security

- Protect sensitive API operations with appropriate authentication.
- Do not expose an unauthenticated arbitrary-download endpoint.
- Validate URL schemes and reject unsupported schemes.
- Mitigate server-side request forgery (SSRF).
- Restrict access to loopback, private, link-local, and cloud metadata addresses unless explicitly required and safely configured.
- Revalidate resolved addresses and redirect targets to prevent bypassing network restrictions.
- Account for DNS rebinding and redirects when enforcing outbound network restrictions.
- Never trust a hostname or URL solely because it was initially validated.

### Filesystem security

- Prevent path traversal and writes outside configured directories.
- Treat provider-supplied filenames and metadata as untrusted.
- Avoid unsafe symlink following and unintended overwrites.
- Validate destination paths before moving or creating files.
- Preserve existing files when a destination collision occurs unless an explicit, safe policy says otherwise.

### Credentials and privacy

- Never commit real credentials.
- Do not expose secrets through logs, API responses, or error messages.
- Avoid logging full URLs containing tokens or credentials.
- Document secure configuration and remote-access practices.
- Do not assume Docker networking alone provides adequate application security.

Do not disable security checks merely to simplify implementation or testing.

## 7. Rust Engineering Standards

- Follow idiomatic Rust conventions and the repository's established style.
- Prefer safe Rust.
- Use explicit error handling and meaningful error types.
- Keep dependencies minimal and justified.
- Avoid unnecessary cloning, allocations, and blocking operations.
- Bound asynchronous tasks and resource consumption.
- Keep public interfaces small and cohesive.
- Use traits where they support genuine replaceability or testing.
- Avoid abstraction for its own sake.
- Respect the repository's existing Rust edition, toolchain, and dependency choices.
- Run `cargo fmt`, `cargo clippy`, and relevant tests when available.

Do not introduce a new web framework, async runtime, database, or frontend stack without first inspecting the existing codebase and evaluating the trade-offs.

If the repository is empty, propose a simple stack and explain the main choices before establishing project-wide conventions.

## 8. Testing Strategy

Every meaningful feature must include appropriate tests.

Prioritize:

- Unit tests for state transitions and URL validation.
- Unit tests for provider detection and capability handling.
- Tests for retry behavior and interrupted transfers.
- Persistence and queue recovery tests.
- API validation and error-response tests.
- Media filename parsing and metadata matching tests.
- Filesystem safety and destination collision tests.
- Security tests for SSRF, redirects, path traversal, and unsafe filenames.

Tests should be deterministic and must not require real provider accounts or personal credentials.

Use mock providers and local test servers where practical.

External metadata services (and Telegram, once implemented) should be mocked in ordinary automated tests. Real integration tests must be explicitly opt-in.

A feature is not complete merely because the code compiles. Verify relevant failure cases and regression risks.

## 9. Observability and Operations

Direkt must be maintainable as a headless, long-running service.

Provide:

- Structured logs.
- Actionable error messages.
- A health endpoint.
- Clear download status and progress reporting.
- Useful context for diagnosing failed transfers.
- Graceful shutdown.
- Persistent queue recovery.
- Disk-space monitoring and warnings.
- Documented operational and troubleshooting procedures.

Avoid logging secrets or unnecessarily sensitive URLs.

Do not introduce a separate monitoring stack or external observability service as an MVP requirement.

## 10. Development Workflow for Claude Code

Before modifying the repository:

1. Inspect the repository structure and existing implementation.
2. Read relevant code, tests, dependencies, and configuration.
3. Identify established conventions and reuse them.
4. Understand how the proposed change interacts with the download lifecycle.
5. Identify relevant failure modes, security implications, and migration requirements.

When implementing a feature:

1. Describe the intended behavior and important design decisions.
2. Make focused, incremental changes.
3. Add or update tests.
4. Update documentation and configuration examples as needed.
5. Run relevant formatting, linting, and tests.
6. Report the changes, checks performed, and known limitations.

Never claim that a command or test was executed unless it actually was.

Do not silently introduce unrelated features, broad refactors, or breaking changes.

Do not assume files, dependencies, APIs, or features exist without inspecting the repository.

When requirements are ambiguous, choose the simplest safe behavior consistent with the project goals. Ask for clarification when a decision has significant architectural, security, data-integrity, or compatibility consequences.

## 11. Definition of Done

A feature is complete when:

- Its behavior matches the stated requirements.
- Its implementation fits the existing architecture.
- Relevant tests have been added or updated.
- Error paths and recovery behavior have been considered.
- Security implications have been addressed.
- Configuration and documentation are updated where necessary.
- Relevant formatting, linting, and tests have been run.
- Known limitations are documented.

Do not mark a feature as complete when important failure paths remain unhandled.

## 12. Product Priorities

When making engineering trade-offs, prioritize:

1. Data integrity and correctness.
2. Security and safe defaults.
3. Reliable downloads and restart recovery.
4. Automatic media organization.
5. A clear web UI and documented API.
6. Straightforward deployment and maintenance.
7. Extensibility for demonstrated needs.
8. Resource efficiency and implementation simplicity.

Direkt should be a dependable self-hosted download service, not merely a collection of scripts that download files.

**The product goal:** A user submits a URL, Direkt reliably downloads it, organizes it when possible, and reports completion or the need for intervention without requiring manual file management.
