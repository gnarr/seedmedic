/**
 * The repair list, and the review queue — the same screen with a different
 * default filter, because they are the same data answered by a different
 * question.
 *
 * One deliberate behaviour: **a live update never reorders the list.** New and
 * changed rows are marked and a "Reorder" control appears instead. On a screen
 * whose buttons include *Abandon and discard staged files*, moving a row out from
 * under a thumb is a safety problem, not a polish problem.
 */

import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { PROGRESSION, api, type Job, type JobList, type RepairState } from "../api";
import { Link, navigate, type LiveStatus } from "../app";
import { bytes, deadline, relative } from "../format";
import {
  Badge,
  Button,
  Card,
  Dialog,
  EmptyState,
  Progress,
  Skeleton,
  StateChip,
} from "../ui";

const ALL_STATES: RepairState[] = [...PROGRESSION, "awaiting_review", "failed"];

function JobRow({
  job,
  selected,
  onSelect,
  selecting,
  fresh,
}: {
  job: Job;
  selected: boolean;
  onSelect: (id: number, on: boolean) => void;
  selecting: boolean;
  fresh: boolean;
}) {
  const due = deadline(job.deadline);
  const recheck = job.state === "rechecking";

  return (
    // A persistent left border rather than a fade, so "this changed" still reads
    // under `prefers-reduced-motion` and after an animation would have ended.
    <Card as="li" className={fresh ? "min-w-0 border-l-4" : "min-w-0"}>
      <div className="flex items-start gap-3">
        {selecting && (
          <input
            type="checkbox"
            checked={selected}
            onChange={(event) => onSelect(job.id, event.target.checked)}
            aria-label={`Select ${job.torrent_name}`}
            className="mt-1 size-5 shrink-0"
          />
        )}

        <div className="min-w-0 flex-1">
          <div className="flex flex-wrap items-center gap-2">
            <StateChip state={job.state} size="sm" />
            {fresh && <Badge tone="accent">updated</Badge>}
            {due && (
              <Badge tone={due.urgency === "past" ? "failed" : due.urgency === "soon" ? "review" : "neutral"}>
                {due.text}
              </Badge>
            )}
          </div>

          <Link
            to={`/repairs/${job.id}`}
            className="mt-1 flex min-h-11 items-center font-medium break-path no-underline"
            style={{ color: "var(--text)" }}
          >
            {job.torrent_name}
          </Link>

          <p className="mt-0.5 text-[13px] break-path" style={{ color: "var(--text-muted)" }}>
            {/* The server's one-line explanation — the same prose as
                ReviewReason::description when parked. */}
            {job.explain}
          </p>

          <p className="nums mt-1 flex flex-wrap gap-x-2 text-[12px]" style={{ color: "var(--text-muted)" }}>
            <span>{job.tracker}</span>
            <span aria-hidden="true">·</span>
            <span>{bytes(job.total_bytes)}</span>
            {job.state_rank !== null && (
              <>
                <span aria-hidden="true">·</span>
                <span>
                  step {job.state_rank + 1} of {job.state_total}
                </span>
              </>
            )}
            <span aria-hidden="true">·</span>
            <time dateTime={job.updated_at}>{relative(job.updated_at)}</time>
          </p>

          {recheck && (
            <div className="mt-2">
              <Progress value={null} label="Checking staged data" tone="rechecking" />
            </div>
          )}
        </div>
      </div>
    </Card>
  );
}

export function RepairsScreen({
  query,
  reviewOnly = false,
  notify,
  onError,
  live,
}: {
  query: URLSearchParams;
  reviewOnly?: boolean;
  notify: (tone: "success" | "danger" | "info", message: string, detail?: string) => void;
  onError: (error: unknown) => void;
  live: LiveStatus;
}) {
  const [list, setList] = useState<JobList | null>(null);
  const [loading, setLoading] = useState(true);
  const [selecting, setSelecting] = useState(false);
  const [selected, setSelected] = useState<Set<number>>(new Set());
  const [search, setSearch] = useState(query.get("q") ?? "");
  const [confirm, setConfirm] = useState<null | "retry" | "abandon">(null);
  const [bulkReport, setBulkReport] = useState<null | { message: string; rows: string[] }>(null);
  /** Rows whose state changed since the last explicit ordering. */
  const [fresh, setFresh] = useState<Set<number>>(new Set());
  const [pendingReorder, setPendingReorder] = useState(false);
  const knownStates = useRef(new Map<number, RepairState>());

  const states = useMemo(
    () => (reviewOnly ? ["awaiting_review"] : query.getAll("state")),
    [reviewOnly, query],
  );

  const load = useCallback(
    async (reorder: boolean) => {
      try {
        const next = await api.jobs({
          state: states,
          q: search || undefined,
          limit: "100",
        });

        // Compare against what we last showed. A row whose state moved is marked;
        // the *order* only changes when the operator asks.
        const changed = new Set<number>();
        for (const job of next.jobs) {
          const before = knownStates.current.get(job.id);
          if (before !== undefined && before !== job.state) changed.add(job.id);
        }

        setList((current) => {
          if (reorder || current === null) {
            knownStates.current = new Map(next.jobs.map((job) => [job.id, job.state]));
            setFresh(changed);
            setPendingReorder(false);
            return next;
          }

          // Keep the order the operator is looking at: re-project the new data
          // onto the old sequence, then append anything genuinely new.
          const byId = new Map(next.jobs.map((job) => [job.id, job]));
          const stable = current.jobs
            .map((job) => byId.get(job.id))
            .filter((job): job is Job => job !== undefined);
          const seen = new Set(stable.map((job) => job.id));
          const added = next.jobs.filter((job) => !seen.has(job.id));

          if (added.length > 0 || stable.length !== current.jobs.length) setPendingReorder(true);
          for (const job of added) changed.add(job.id);
          setFresh((previous) => new Set([...previous, ...changed]));
          knownStates.current = new Map(next.jobs.map((job) => [job.id, job.state]));

          return { ...next, jobs: [...stable, ...added] };
        });
      } catch (error) {
        onError(error);
      } finally {
        setLoading(false);
      }
    },
    [states, search, onError],
  );

  useEffect(() => {
    void load(true);
  }, [load]);

  // A live event refreshes the data but not the ordering.
  useEffect(() => {
    if (live === "live") void load(false);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [live]);

  const toggle = (id: number, on: boolean) =>
    setSelected((current) => {
      const next = new Set(current);
      if (on) next.add(id);
      else next.delete(id);
      return next;
    });

  const runBulk = async (action: "retry" | "abandon") => {
    setConfirm(null);
    try {
      const response = await api.bulk(action, [...selected]);
      const failures = response.results.filter((result) => !result.ok);
      // One summary, never twenty toasts.
      notify(
        failures.length === 0 ? "success" : "info",
        `${response.applied} of ${response.total} repair${response.total === 1 ? "" : "s"} ${
          action === "retry" ? "retried" : "abandoned"
        }.`,
        failures.length > 0 ? `${failures.length} could not be — see details.` : undefined,
      );
      if (failures.length > 0) {
        setBulkReport({
          message: `${failures.length} repair${failures.length === 1 ? "" : "s"} could not be ${action === "retry" ? "retried" : "abandoned"}`,
          rows: failures.map((f) => `#${f.id} — ${f.message ?? "refused"}`),
        });
      }
      setSelected(new Set());
      setSelecting(false);
      void load(true);
    } catch (error) {
      onError(error);
    }
  };

  const title = reviewOnly ? "Needs a decision" : "Repairs";

  return (
    <>
      <div className="mb-3 flex flex-wrap items-baseline justify-between gap-2">
        <h1 className="text-[22px] font-semibold tracking-tight">{title}</h1>
        {list && (
          <p className="nums text-[13px]" style={{ color: "var(--text-muted)" }}>
            {list.total_matching} matching
          </p>
        )}
      </div>

      {!reviewOnly && (
        <>
          <form
            className="mb-3 flex gap-2"
            onSubmit={(event) => {
              event.preventDefault();
              void load(true);
            }}
          >
            <label className="sr-only" htmlFor="repair-search">
              Search repairs by name or info-hash
            </label>
            <input
              id="repair-search"
              type="search"
              value={search}
              onChange={(event) => setSearch(event.target.value)}
              placeholder="Search by name, or paste an info-hash"
              className="min-h-[var(--control-h)] min-w-0 flex-1 rounded-[var(--radius-md)] border px-3 text-[14px]"
              style={{ background: "var(--surface)", borderColor: "var(--border-strong)", color: "var(--text)" }}
            />
            <Button type="submit">Search</Button>
          </form>

          {/* The one place a horizontal scroll is allowed, and it is inside its
              own box rather than the page. */}
          <div className="scroll-x -mx-4 mb-4 px-4" data-allow-xscroll>
            <ul className="flex w-max gap-2 pb-1">
              <li>
                <Link
                  to="/repairs"
                  aria-current={states.length === 0 ? "page" : undefined}
                  className="inline-flex min-h-11 items-center rounded-full border px-3 text-[13px] no-underline"
                  style={{
                    color: states.length === 0 ? "var(--accent-on-soft)" : "var(--text-muted)",
                    borderColor: states.length === 0 ? "var(--accent)" : "var(--border)",
                    background: states.length === 0 ? "var(--accent-soft)" : "transparent",
                  }}
                >
                  All
                </Link>
              </li>
              {ALL_STATES.map((state) => {
                const on = states.includes(state);
                return (
                  <li key={state}>
                    <Link
                      to={`/repairs?state=${state}`}
                      aria-current={on ? "page" : undefined}
                      className="no-underline"
                    >
                      <span style={{ opacity: on ? 1 : 0.65 }}>
                        <StateChip state={state} size="sm" />
                      </span>
                    </Link>
                  </li>
                );
              })}
            </ul>
          </div>
        </>
      )}

      {pendingReorder && (
        <div className="mb-3">
          <Button
            onClick={() => void load(true)}
            variant="secondary"
            block
          >
            Repairs changed — reorder the list
          </Button>
        </div>
      )}

      {loading && !list ? (
        <Skeleton rows={4} />
      ) : !list || list.jobs.length === 0 ? (
        <Card>
          {reviewOnly ? (
            <EmptyState glyph="✓" title="Nothing needs a decision">
              Every repair is either running on its own or finished. SeedMedic only stops to ask when
              guessing would risk your library.
            </EmptyState>
          ) : (
            <EmptyState glyph="🌱" title="No repairs match">
              {states.length > 0 || search ? (
                <>
                  Nothing here with that filter. <Link to="/repairs">Show everything</Link>.
                </>
              ) : (
                "Nothing discovered yet — SeedMedic polls each tracker on a timer."
              )}
            </EmptyState>
          )}
        </Card>
      ) : (
        <>
          <div className="mb-2 flex flex-wrap items-center gap-2">
            <Button
              onClick={() => {
                setSelecting((on) => !on);
                setSelected(new Set());
              }}
              aria-pressed={selecting}
            >
              {selecting ? "Done selecting" : "Select"}
            </Button>
            {selecting && (
              <>
                <Button
                  disabled={selected.size === 0}
                  onClick={() => setConfirm("retry")}
                >
                  Retry {selected.size > 0 && `(${selected.size})`}
                </Button>
                <Button
                  variant="danger"
                  disabled={selected.size === 0}
                  onClick={() => setConfirm("abandon")}
                >
                  Abandon {selected.size > 0 && `(${selected.size})`}
                </Button>
              </>
            )}
          </div>

          <ul className="space-y-2">
            {list.jobs.map((job) => (
              <JobRow
                key={job.id}
                job={job}
                selected={selected.has(job.id)}
                onSelect={toggle}
                selecting={selecting}
                fresh={fresh.has(job.id)}
              />
            ))}
          </ul>
        </>
      )}

      <Dialog
        open={confirm !== null}
        onClose={() => setConfirm(null)}
        title={confirm === "abandon" ? "Abandon these repairs?" : "Retry these repairs?"}
        destructive={confirm === "abandon"}
        footer={
          <Button
            variant={confirm === "abandon" ? "danger" : "primary"}
            onClick={() => confirm && void runBulk(confirm)}
          >
            {confirm === "abandon" ? "Abandon" : "Retry"} {selected.size}
          </Button>
        }
      >
        {confirm === "abandon" ? (
          <p>
            {selected.size} repair{selected.size === 1 ? "" : "s"} will be marked failed. Staged files
            are <strong>not</strong> deleted — discard those individually if you want the space back.
          </p>
        ) : (
          <p>
            Each of the {selected.size} selected repair{selected.size === 1 ? "" : "s"} resumes at the
            exact step it stopped at. Any that cannot are reported and left alone.
          </p>
        )}
      </Dialog>

      <Dialog
        open={bulkReport !== null}
        onClose={() => setBulkReport(null)}
        title={bulkReport?.message ?? ""}
      >
        <ul className="space-y-1.5">
          {bulkReport?.rows.map((row) => (
            <li key={row} className="break-path">
              {row}
            </li>
          ))}
        </ul>
      </Dialog>
    </>
  );
}

/** Used by the job screen's back link so both agree on where "back" is. */
export function backToList() {
  navigate("/repairs");
}
