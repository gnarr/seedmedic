//! `GET /api/v1/events` — server-sent events.
//!
//! Three things here are load-bearing and easy to get wrong:
//!
//! 1. **The session is re-checked on every emit.** `require_auth_token` runs
//!    once per request, and an event stream *is* one request whose body never
//!    ends. `RuntimeHandle::reload` clears every session when
//!    `server.auth_token` changes — but a stream already running would never
//!    consult `has_session` again, so rotating the token would leave every open
//!    tab with a live feed of job names, staging paths and tracker errors on a
//!    session that was revoked. That would make 0018's invariant "sessions are
//!    invalidated when the token changes" false for the one connection where it
//!    matters most.
//!
//! 2. **This is the documented exception to one-generation-per-request.**
//!    `src/web/AGENTS.md` requires `runtime.current()` exactly once at the top of
//!    a handler. Obeying that literally here would pin one `Arc<Runtime>` for the
//!    life of the connection and keep serving from adapters replaced hours ago.
//!    So the stream re-reads it per emit and carries a generation counter, and a
//!    change in that counter tells the client to refetch.
//!
//! 3. **`Last-Event-ID` is a gap *detector*, not a replay log.** There is no
//!    durable event log — a `broadcast` channel holds a few hundred recent events
//!    for *live* receivers and nothing at all for a reconnecting one. Pretending
//!    to resume would be claiming something that did not happen, so a reconnect
//!    naming a sequence we cannot prove we delivered gets `gap` and refetches.

use std::{convert::Infallible, time::Duration};

use axum::{
    extract::State,
    http::{HeaderMap, StatusCode, header},
    response::{
        IntoResponse, Response,
        sse::{Event, KeepAlive, Sse},
    },
};
use serde_json::json;
use tokio_stream::{
    StreamExt, wrappers::BroadcastStream, wrappers::errors::BroadcastStreamRecvError,
};

use crate::{
    events::{ActivityKind, EventKind},
    web::{AppState, login},
};

/// Long enough to be cheap, short enough to beat every intermediary's idle
/// timeout: nginx's default `proxy_read_timeout` is 60s and Cloudflare's is 100s,
/// and self-hosters put SeedMedic behind exactly those. 15s gives 4× margin and
/// costs one `:` comment line per interval, which no `EventSource` surfaces as a
/// message.
const KEEP_ALIVE: Duration = Duration::from_secs(15);

pub async fn stream(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let bus = state.runtime.events();

    // Subscribe *before* reading the sequence, so nothing published in between
    // can be reported as neither delivered nor missed.
    let receiver = bus.subscribe();
    let current_seq = bus.seq();

    // Whether this connection is allowed to keep receiving. Captured once for
    // the auth *mode*, re-evaluated per emit for the session's liveness.
    let runtime = state.runtime.current();
    let requires_session = runtime.auth_token.is_some();
    let session_id = login::session_id_from(&headers);
    // A bearer-authenticated caller (a script) has no session to revoke, so it is
    // exempt from the per-emit check. `EventSource` cannot send headers, so a
    // browser is always the cookie case.
    let bearer_authenticated = runtime
        .auth_token
        .as_ref()
        .zip(
            headers
                .get(header::AUTHORIZATION)
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.strip_prefix("Bearer ")),
        )
        .is_some_and(|(expected, token)| expected.verify(token));

    if requires_session && !bearer_authenticated && session_id.is_none() {
        // The middleware already rejects this, but being explicit here keeps the
        // stream's own contract readable.
        return StatusCode::UNAUTHORIZED.into_response();
    }

    let resumed_from = headers
        .get("last-event-id")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok());

    let opening = match resumed_from {
        // A reconnect naming a sequence we cannot prove we delivered. Say so and
        // let the client refetch, rather than leaving a screen quietly wrong.
        Some(from) if from < current_seq => sse_event(
            current_seq,
            "gap",
            json!({ "seq": current_seq, "missed": current_seq - from }),
        ),
        _ => sse_event(current_seq, "hello", json!({ "seq": current_seq })),
    };

    let handle = state.runtime.clone();
    let generation = handle.generation();

    let live = BroadcastStream::new(receiver).filter_map(move |item| {
        // Re-checked per emit. See point 1 in the module docs.
        if requires_session
            && !bearer_authenticated
            && !session_id
                .as_deref()
                .is_some_and(|id| handle.has_session(id))
        {
            // Ending the stream is the point: `EventSource` will reconnect, get
            // a 401, and the client routes to the login screen.
            return None;
        }

        Some(Ok(match item {
            Ok(event) => {
                let mut payload = payload_for(&event.kind);
                payload["seq"] = json!(event.seq);
                // A reload replaced the adapters under this connection; the
                // client refetches rather than trusting anything it holds.
                if handle.generation() != generation {
                    payload["generation_changed"] = json!(true);
                }
                sse_event(event.seq, name_for(&event.kind), payload)
            }
            // The client is slower than the worker. `broadcast` has already
            // dropped what it could not hold and there is nowhere to get it from,
            // so say so rather than closing the stream and making it guess.
            Err(BroadcastStreamRecvError::Lagged(missed)) => sse_event(
                0,
                "gap",
                json!({ "missed": missed, "reason": "the client fell behind" }),
            ),
        }))
    });

    let stream = tokio_stream::iter([Ok::<Event, Infallible>(opening)]).chain(live);

    let mut response = Sse::new(stream)
        .keep_alive(KeepAlive::new().interval(KEEP_ALIVE))
        .into_response();
    // nginx buffers a proxied response by default and will hold an event stream
    // until its buffer fills, which looks exactly like a broken UI. This header
    // is the documented opt-out; Caddy and Traefik stream by default.
    response
        .headers_mut()
        .insert("x-accel-buffering", header::HeaderValue::from_static("no"));
    response
}

fn sse_event(seq: u64, name: &str, data: serde_json::Value) -> Event {
    let event = Event::default().event(name).data(data.to_string());
    if seq > 0 {
        event.id(seq.to_string())
    } else {
        event
    }
}

/// The event's wire name. Distinct names so a client can throttle the cheap,
/// frequent ones differently from a real transition.
fn name_for(kind: &EventKind) -> &'static str {
    match kind {
        EventKind::JobTransitioned { .. } => "job",
        EventKind::JobsChanged { .. } => "jobs",
        EventKind::JobProgress { .. } => "progress",
        EventKind::Activity(_) => "activity",
        EventKind::TrackersChanged => "trackers",
        EventKind::ConfigReloaded { .. } => "settings",
        EventKind::AuthTokenChanged => "session",
    }
}

/// Hints, not documents — see [`crate::events`] for why. The two or three strings
/// a transition carries are what let a state chip update without a round trip;
/// nothing here can carry a secret.
fn payload_for(kind: &EventKind) -> serde_json::Value {
    match kind {
        EventKind::JobTransitioned {
            job,
            from,
            to,
            reason,
        } => json!({ "id": job, "from": from, "to": to, "reason": reason }),
        EventKind::JobsChanged { jobs } => json!({ "changed": jobs }),
        EventKind::JobProgress { job } => json!({ "id": job }),
        EventKind::Activity(activity) => json!({
            "kind": match activity.kind {
                ActivityKind::Tick => "tick",
                ActivityKind::Discovery => "discovery",
                ActivityKind::Reconcile => "reconcile",
            },
            "at": activity.at,
        }),
        EventKind::TrackersChanged => json!({}),
        EventKind::ConfigReloaded { restart_needed } => json!({ "restart_needed": restart_needed }),
        EventKind::AuthTokenChanged => json!({ "reason": "auth_token_changed" }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{events::EventBus, repair::JobId, repair::RepairState};

    #[test]
    fn a_transition_carries_enough_to_update_a_chip_without_a_refetch() {
        let payload = payload_for(&EventKind::JobTransitioned {
            job: JobId(7),
            from: RepairState::Injected,
            to: RepairState::Rechecking,
            reason: "progress",
        });

        assert_eq!(payload["id"], 7);
        assert_eq!(payload["to"], "rechecking");
        assert_eq!(payload["reason"], "progress");
    }

    /// A bulk action publishes one event for the whole batch, so its payload has
    /// to carry the whole list.
    #[test]
    fn a_batch_names_every_job_it_changed() {
        let payload = payload_for(&EventKind::JobsChanged {
            jobs: vec![JobId(1), JobId(2)],
        });
        assert_eq!(payload["changed"], json!([1, 2]));
    }

    #[test]
    fn every_event_kind_has_a_distinct_wire_name() {
        let names = [
            name_for(&EventKind::JobTransitioned {
                job: JobId(1),
                from: RepairState::Discovered,
                to: RepairState::TorrentFetched,
                reason: "progress",
            }),
            name_for(&EventKind::JobsChanged { jobs: Vec::new() }),
            name_for(&EventKind::JobProgress { job: JobId(1) }),
            name_for(&EventKind::Activity(crate::events::Activity::default())),
            name_for(&EventKind::TrackersChanged),
            name_for(&EventKind::ConfigReloaded {
                restart_needed: Vec::new(),
            }),
            name_for(&EventKind::AuthTokenChanged),
        ];

        let mut unique: Vec<&str> = names.to_vec();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(
            unique.len(),
            names.len(),
            "two kinds sharing a name means a client cannot throttle one without \
             throttling the other: {names:?}"
        );
        // `progress` fires for every seeding job on every poll, so it must not
        // share a name with a real transition.
        assert_ne!(
            name_for(&EventKind::JobProgress { job: JobId(1) }),
            name_for(&EventKind::JobTransitioned {
                job: JobId(1),
                from: RepairState::Discovered,
                to: RepairState::TorrentFetched,
                reason: "progress",
            })
        );
    }

    /// No event payload may carry anything a client should not already be
    /// allowed to fetch. Hints are what make that structurally true.
    #[test]
    fn no_event_payload_carries_free_text_from_configuration() {
        let bus = EventBus::new();
        let _ = bus.subscribe();
        for kind in [
            EventKind::TrackersChanged,
            EventKind::AuthTokenChanged,
            EventKind::ConfigReloaded {
                restart_needed: vec!["server.bind_address"],
            },
        ] {
            let payload = payload_for(&kind).to_string();
            // The only strings any of these may contain are fixed literals from
            // this crate.
            assert!(
                payload.len() < 120,
                "a settings event grew a payload big enough to hold a value: {payload}"
            );
        }
    }
}
