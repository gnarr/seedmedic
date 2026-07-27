# 0012 — Structured logging, metrics, and diagnostics

**Status:** Not started
**Depends on:** 0001
**Blocks:** nothing

## Problem

Logging is structured and quiet, which is a good start, but operating SeedMedic
means answering questions it currently cannot:

- Is anything stuck? A job waiting for a tracker and a job wedged in a
  rewind/advance loop look identical from outside.
- How long does a repair take, and where does the time go?
- Is the tracker rate-limiting us? Is qBittorrent reachable?
- Why did *this* repair choose *that* file? The evidence is in the audit trail
  but only reachable through the job detail page.
- `/health` returns `"ok"` unconditionally — it proves the process is listening
  and nothing else, so it is useless as a readiness probe.

There is also no correlation: a repair's log lines are scattered across ticks
with no shared identifier beyond `job=`.

## Architectural context

`tracing` is initialised in `main::init_logging` with an `EnvFilter`. The
durable record of *decisions* is `repair_job_transitions`, not the log — that
stays true. Logs are for operating; the audit trail is for explaining.

`TickSummary` already counts what happened in a tick and is currently discarded.

## Expected behaviour

- Every log line for a repair carries the job id, the tracker, and the state,
  without each call site remembering to add them.
- A meaningful `/health` distinguishing "process alive" from "able to work".
- A diagnostics page showing what SeedMedic can currently reach and what it is
  working on.
- Optional Prometheus metrics for people who want graphs.
- Optional notification on the events an operator actually cares about: a repair
  parked for review, a repair completed, a tracker unreachable.

## Implementation steps

1. **Spans.** Wrap `RepairWorker::drive` in
   `tracing::info_span!("repair", job = %id, tracker = %job.tracker)` and each
   step in a child span named for the state. Every line inside inherits the
   fields, and per-step timing comes free from span durations.

2. **Use `TickSummary`.** Log it at `debug` every tick and at `info` when
   anything non-trivial happened. It already counts advanced, parked, waiting,
   retrying, and rewound.

3. **Real `/health`.** Return `200` when the database is reachable and the
   worker has ticked recently, `503` otherwise. "Recently" needs a
   last-tick timestamp somewhere shared — an `AtomicI64` on the app state is
   enough. Do *not* make health depend on tracker or client reachability: those
   being down is a normal, recoverable condition, and a health check that fails
   on it will get the container restarted for no reason.

4. **Diagnostics page.** `/status`, rendered with maud like the rest:
   - Counts by state, with review broken down by reason.
   - Per-tracker: last successful poll, last error, whether the adapter is a
     stub.
   - Download client: reachable, torrent count, whether it is a stub.
   - Staging root: path, free space, bytes held by SeedMedic.
   - Effective policy, with secrets redacted.

   This is the page somebody links in a bug report, so make it complete and
   copy-pasteable.

5. **Metrics, optional.** Behind a `metrics` feature and a config flag:
   repairs by state, transitions by from/to, step durations, external calls by
   outcome, staged bytes. Prefer the `metrics` facade over a specific exporter.
   Do not add this by default — most self-hosted users will never scrape it.

6. **Notifications, optional.** A generic webhook (Apprise-compatible, or plain
   JSON POST) for: parked for review, completed, tracker unreachable for longer
   than a threshold. Keep it one small adapter behind a port, and keep the
   event list short.

7. **Stuck detection.** With 0001's oscillation warning in place, surface it:
   a job that has rewound more than N times, or has been in one state past a
   threshold, is flagged on the status page.

## Invariants and safety constraints

- Never log a secret, a password, or an API key — including inside a URL. The
  diagnostics page renders `Secret` through its redacting `Debug`.
- The diagnostics page is read-only. No actions.
- Logs are not the durable record. Anything needed to explain a decision goes in
  the transition `detail`, and this document does not change that.
- Health must not fail because an external system is down. That is a normal
  state, not an unhealthy one.
- Metrics and notifications are optional and off by default; neither may become
  a dependency of the repair workflow.

## Likely files

- `src/main.rs` (logging setup)
- `src/repair/worker.rs` (spans, tick summary)
- `src/web/mod.rs`, `src/web/status.rs` (new)
- `src/web/health.rs` (new)
- `src/config.rs`, `config.example.toml`
- `Cargo.toml` (optional features)

## Required tests

- A log line emitted inside a step carries the job id from the span.
- `/health` returns `503` when the worker has not ticked within the threshold,
  and `200` when it has.
- `/health` still returns `200` with every tracker unreachable.
- The status page renders with zero jobs, with jobs in every state, and with a
  stub adapter configured.
- No secret appears in the status page HTML — assert on the string.
- With metrics disabled, no metrics code runs (a compile-time check via the
  feature).

## Acceptance criteria

- A repair's whole life can be followed with `RUST_LOG=seedmedic=debug` and one
  `grep` on the job id.
- `/status` answers "what is SeedMedic doing and can it reach everything?" on one
  page.
- A container orchestrator's health check does not restart the service because a
  tracker had a bad afternoon.

## Out of scope

- Distributed tracing, OpenTelemetry export. One process, one machine.
- Log shipping.
- An alerting engine. A webhook is the boundary; whatever receives it can have
  the rules.

## Open questions

- What is the right staleness threshold for `/health`? It has to exceed
  `worker.poll_interval` with margin, so derive it rather than hard-coding.
- Should the status page show recent transitions across all jobs — a global
  activity feed? Useful, and it is another query and another thing to keep fast.
- Prometheus, or a plain JSON `/metrics`? JSON avoids a dependency; Prometheus is
  what people expect.
- Are per-step durations worth persisting, so the status page can show "matching
  usually takes 2s, this one took 400s"? The audit trail has timestamps; the
  arithmetic is a query away.
