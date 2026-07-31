/**
 * The dashboard: what needs me, what is happening, is anything broken.
 *
 * Signals only. Every number here comes from a bounded query, so it is safe to
 * refetch on every event — the detail that costs a filesystem walk lives on
 * `/diagnostics`.
 */

import type { Dashboard } from "../api";
import { Link, navigate } from "../app";
import { relative } from "../format";
import { Badge, Button, Card, EmptyState, LinkButton, SectionHeading, Skeleton, StateChip } from "../ui";

function Tile({
  label,
  value,
  tone,
  to,
}: {
  label: string;
  value: number;
  tone: string;
  to?: string;
}) {
  const body = (
    <>
      <p className="text-[13px]" style={{ color: "var(--text-muted)" }}>
        {label}
      </p>
      <p className="nums mt-0.5 text-[28px] leading-none font-semibold" style={{ color: `var(--state-${tone})` }}>
        {value}
      </p>
    </>
  );

  return (
    <Card className="min-w-0">
      {to ? (
        <Link to={to} className="block no-underline" style={{ color: "var(--text)" }}>
          {body}
        </Link>
      ) : (
        body
      )}
    </Card>
  );
}

export function DashboardScreen({
  dashboard,
  onRefresh,
}: {
  dashboard: Dashboard | null;
  onRefresh: () => void;
}) {
  // Real `href`s so middle-click and open-in-new-tab work, with the click
  // intercepted for client-side navigation.
  const reviewHref = `${new URL(document.baseURI).pathname.replace(/\/$/, "")}/review`;
  if (!dashboard) {
    return (
      <>
        <h1 className="mb-3 text-[22px] font-semibold tracking-tight">Dashboard</h1>
        <Skeleton rows={4} />
      </>
    );
  }

  const { counts, attention, worker, trackers } = dashboard;
  const active = counts.by_state
    .filter(({ state }) => !["completed", "failed", "awaiting_review"].includes(state))
    .reduce((total, { count }) => total + count, 0);
  const done = counts.by_state.find(({ state }) => state === "completed")?.count ?? 0;

  return (
    <>
      <h1 className="mb-3 text-[22px] font-semibold tracking-tight">Dashboard</h1>

      {counts.total === 0 ? (
        <Card>
          <EmptyState glyph="🌱" title="No hit-and-runs discovered yet">
            SeedMedic polls each configured tracker on a timer. When a warning shows up, the repair
            appears here on its own.
          </EmptyState>
        </Card>
      ) : (
        <>
          {attention.review > 0 && (
            <Card className="mb-4" >
              <div className="flex flex-wrap items-center justify-between gap-3">
                <div className="min-w-0">
                  <p className="font-semibold" style={{ color: "var(--state-review)" }}>
                    ⚠ {attention.review} repair{attention.review === 1 ? "" : "s"} need a decision
                  </p>
                  <p className="text-[14px]" style={{ color: "var(--text-muted)" }}>
                    A repair that stops and asks is working as intended.
                  </p>
                </div>
                <LinkButton
                  href={reviewHref}
                  variant="primary"
                  onClick={(event) => {
                    event.preventDefault();
                    navigate("/review");
                  }}
                >
                  Review now
                </LinkButton>
              </div>
            </Card>
          )}

          <div className="mb-5 grid grid-cols-2 gap-3 sm:grid-cols-4">
            <Tile label="Active" value={active} tone="seeding" to="/repairs" />
            <Tile label="Review" value={attention.review} tone="review" to="/review" />
            <Tile label="Done" value={done} tone="completed" to="/repairs?state=completed" />
            <Tile label="Failed" value={attention.failed} tone="failed" to="/repairs?state=failed" />
          </div>

          {attention.stuck.length > 0 && (
            <section className="mb-5">
              <SectionHeading>Possibly stuck</SectionHeading>
              <ul className="space-y-2">
                {attention.stuck.map((stuck) => (
                  <Card as="li" key={stuck.job}>
                    <Link
                      to={`/repairs/${stuck.job}`}
                      className="flex min-h-11 items-center font-medium break-path"
                    >
                      {stuck.torrent_name}
                    </Link>
                    <p className="text-[14px]" style={{ color: "var(--text-muted)" }}>
                      {stuck.detail}
                    </p>
                  </Card>
                ))}
              </ul>
            </section>
          )}

          <section className="mb-5">
            <SectionHeading
              action={
                <Link to="/repairs" className="inline-flex min-h-11 items-center text-[14px]">
                  All repairs →
                </Link>
              }
            >
              By state
            </SectionHeading>
            <Card>
              <ul className="flex flex-wrap gap-2">
                {counts.by_state.map(({ state, count }) => (
                  <li key={state}>
                    <Link
                      to={`/repairs?state=${state}`}
                      className="inline-flex min-h-11 items-center no-underline"
                    >
                      <span className="inline-flex items-center gap-1.5">
                        <StateChip state={state} size="sm" />
                        <span className="nums text-[13px]" style={{ color: "var(--text-muted)" }}>
                          {count}
                        </span>
                      </span>
                    </Link>
                  </li>
                ))}
              </ul>
            </Card>
          </section>

          {counts.by_review_reason.length > 0 && (
            <section className="mb-5">
              <SectionHeading>Waiting on you, by reason</SectionHeading>
              <Card>
                <ul className="space-y-2.5">
                  {counts.by_review_reason.map((group) => (
                    <li key={group.reason ?? "none"} className="flex items-start gap-2">
                      <Badge tone="review">{group.count}</Badge>
                      <span className="min-w-0 flex-1 text-[14px]">
                        {/* The server's own prose, from ReviewReason::description. */}
                        {group.description ?? "No reason recorded."}
                      </span>
                    </li>
                  ))}
                </ul>
              </Card>
            </section>
          )}
        </>
      )}

      <section>
        <SectionHeading
          action={
            <Link to="/diagnostics" className="inline-flex min-h-11 items-center text-[14px]">
              Diagnostics →
            </Link>
          }
        >
          Integrations
        </SectionHeading>
        <Card>
          {trackers.length === 0 ? (
            <p className="text-[14px]" style={{ color: "var(--text-muted)" }}>
              No trackers configured yet.
            </p>
          ) : (
            <ul className="space-y-2">
              {trackers.map((tracker) => {
                const ok = tracker.last_error === null && tracker.last_success !== null;
                const tone = ok ? "completed" : tracker.last_error ? "failed" : "neutral";
                return (
                  <li key={tracker.id} className="flex flex-wrap items-baseline gap-x-2 gap-y-0.5">
                    <span aria-hidden="true" style={{ color: `var(--state-${tone})` }}>
                      {ok ? "●" : tracker.last_error ? "▲" : "○"}
                    </span>
                    <span className="font-medium break-path">{tracker.id}</span>
                    <span className="text-[13px]" style={{ color: "var(--text-muted)" }}>
                      {tracker.adapter}
                      {tracker.stub && " (demo)"} ·{" "}
                      {tracker.last_error
                        ? `error ${relative(tracker.last_error.at)}`
                        : tracker.last_success
                          ? `polled ${relative(tracker.last_success)}`
                          : "never polled"}
                    </span>
                  </li>
                );
              })}
            </ul>
          )}

          <dl className="mt-3 grid grid-cols-[max-content_1fr] gap-x-4 gap-y-1 border-t pt-3 text-[13px]" style={{ borderColor: "var(--border)" }}>
            <dt style={{ color: "var(--text-muted)" }}>Worker</dt>
            <dd className="nums">
              {worker.last_tick ? `last ran ${relative(worker.last_tick)}` : "has not run yet"}
              {worker.last_tick_summary &&
                worker.last_tick_summary.advanced > 0 &&
                ` · advanced ${worker.last_tick_summary.advanced}`}
            </dd>
            <dt style={{ color: "var(--text-muted)" }}>Config</dt>
            <dd className="break-path">{dashboard.setup.config_path}</dd>
          </dl>
        </Card>
      </section>

      <div className="mt-4 flex justify-end">
        <Button onClick={onRefresh}>Refresh</Button>
      </div>
    </>
  );
}
