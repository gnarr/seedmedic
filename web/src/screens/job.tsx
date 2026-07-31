/**
 * One repair: where it stands, what it plans to do, what it has done, and the
 * decision panel when it is waiting on a human.
 *
 * Three things here are the point of the whole rebuild:
 *
 * - **The decision panel comes first**, immediately after the heading, because a
 *   parked repair is the only screen that needs a person.
 * - **The candidate picker shows evidence.** `MatchEvidence` is persisted for
 *   every planned file and was displayed nowhere; it is the "why do we believe
 *   this" an operator needs before approving an ambiguous match.
 * - **The audit trail is a timeline, not a table with JSON in a cell.** Each entry
 *   is a sentence, with the raw payload one disclosure away.
 */

import { useCallback, useEffect, useRef, useState } from "react";
import { ACTION_PATH, api, type ActionName, type JobDetail, type PlannedFile } from "../api";
import { Link, type LiveStatus } from "../app";
import {
  absolute,
  bytes,
  confidenceMeter,
  deadline,
  duration,
  middleTruncate,
  relative,
  stateLabel,
} from "../format";
import {
  Badge,
  Banner,
  Button,
  Card,
  Dialog,
  EmptyState,
  Progress,
  SectionHeading,
  Skeleton,
  StateChip,
  Stepper,
} from "../ui";

/** Success copy per action — the feedback the old redirects never gave. */
const SUCCESS: Record<string, (detail: JobDetail) => string> = {
  retry: (d) => `Retrying from ${stateLabel(d.job.state).toLowerCase()}.`,
  restart: () => "Starting over — the .torrent will be fetched again.",
  abandon: () => "Repair abandoned.",
  "abandon-and-discard": () => "Abandoned, and the staged files were removed.",
  "approve-resume": () => "Resume approved for this repair only. The policy is unchanged.",
  "discard-staging": () => "Staged files removed. That torrent has stopped seeding.",
};

// --- audit trail ----------------------------------------------------------

/**
 * The audit `detail` JSON as a sentence.
 *
 * Every branch is derived from what the step actually writes (see
 * `src/repair/application/*.rs`). Anything unrecognised falls through to the raw
 * disclosure rather than being dropped — the trail exists so a decision can be
 * explained months later, and silently hiding part of it would defeat that.
 */
function describe(reason: string, detail: Record<string, unknown> | null): string | null {
  if (!detail) return null;
  const num = (key: string) => (typeof detail[key] === "number" ? (detail[key] as number) : null);
  const str = (key: string) => (typeof detail[key] === "string" ? (detail[key] as string) : null);

  if (reason === "discovered") {
    const size = num("size_bytes");
    return `The tracker reported a hit-and-run${size !== null ? ` on ${bytes(size)}` : ""}.`;
  }
  if (detail["operator"] !== undefined) {
    const who = String(detail["operator"]).replace(/_/g, " ");
    const discarded = detail["staging_discarded"] === true ? ", discarding staged files" : "";
    return `You chose to ${who}${discarded}.`;
  }
  if (num("files") !== null && num("total_bytes") !== null) {
    return `Read the .torrent: ${num("files")} file(s), ${bytes(num("total_bytes"))}.`;
  }
  if (num("matched") !== null) {
    const unmatched = Array.isArray(detail["unmatched"]) ? (detail["unmatched"] as unknown[]).length : 0;
    const verification = detail["verification"] as { checked?: number } | undefined;
    const checked = verification?.checked;
    return [
      `Matched ${num("matched")} of ${(num("matched") ?? 0) + unmatched} file(s)`,
      checked ? `${checked} piece-verified` : null,
      unmatched > 0 ? `${unmatched} still undecided` : null,
    ]
      .filter(Boolean)
      .join("; ")
      .concat(".");
  }
  if (str("strategy") !== null) {
    const aliases = detail["aliases_library"] === true;
    return `Staged ${bytes(num("bytes"))} by ${str("strategy")}${
      aliases ? " — these files share inodes with your library" : ""
    }.`;
  }
  const completeness = detail["completeness"] as { completeness?: string; ratio?: number } | undefined;
  if (completeness?.completeness) {
    return completeness.completeness === "complete"
      ? "The client confirmed the staged data matches the torrent."
      : `The check found ${Math.round((completeness.ratio ?? 0) * 100)}% — something does not match.`;
  }
  if (num("elapsed_seconds") !== null) {
    return `The check ran for ${duration(num("elapsed_seconds"))} without finishing.`;
  }
  if (num("attempts") !== null) {
    return `Gave up after ${num("attempts")} attempt(s): ${str("error") ?? "unknown error"}.`;
  }
  if (str("note") !== null) return str("note");
  if (str("save_path") !== null) return `Handed to the download client, paused.`;
  return null;
}

function Timeline({ history }: { history: JobDetail["history"] }) {
  return (
    <ol className="space-y-3">
      {[...history].reverse().map((entry, index) => {
        const sentence = describe(entry.reason, entry.detail);
        const moved = entry.from !== entry.to;
        return (
          <li key={`${entry.occurred_at}-${index}`} className="flex gap-3">
            <div className="flex flex-col items-center pt-1.5">
              <span aria-hidden="true" className="size-2 rounded-full" style={{ background: "var(--border-strong)" }} />
              {index < history.length - 1 && (
                <span aria-hidden="true" className="mt-1 w-px flex-1" style={{ background: "var(--border)" }} />
              )}
            </div>
            <div className="min-w-0 flex-1 pb-1">
              <p className="flex flex-wrap items-center gap-x-2 text-[13px]">
                <span className="font-medium">
                  {moved ? (
                    <>
                      {stateLabel(entry.from)} → {stateLabel(entry.to)}
                    </>
                  ) : (
                    stateLabel(entry.to)
                  )}
                </span>
                <time
                  dateTime={entry.occurred_at}
                  title={absolute(entry.occurred_at)}
                  style={{ color: "var(--text-muted)" }}
                >
                  {relative(entry.occurred_at)}
                </time>
              </p>
              {sentence && <p className="text-[14px] break-path">{sentence}</p>}
              {entry.detail && (
                <details className="mt-1">
                  <summary
                    className="inline-flex min-h-11 cursor-pointer items-center text-[13px]"
                    style={{ color: "var(--text-muted)" }}
                  >
                    Show raw record
                  </summary>
                  <pre
                    className="scroll-x mt-1 rounded-[var(--radius-sm)] p-2 text-[12px]"
                    style={{ background: "var(--surface-2)" }}
                    data-allow-xscroll
                  >
                    {JSON.stringify(entry.detail, null, 2)}
                  </pre>
                </details>
              )}
            </div>
          </li>
        );
      })}
    </ol>
  );
}

// --- the file plan --------------------------------------------------------

function FilePlan({ files }: { files: PlannedFile[] }) {
  return (
    // Cards below 768px, a table above. The old six-column table guaranteed
    // horizontal overflow on a phone.
    <>
      <ul className="space-y-2 md:hidden">
        {files.map((file) => {
          const meter = confidenceMeter(file.confidence);
          return (
            <Card as="li" key={file.torrent_path}>
              <p className="font-medium break-path">{file.torrent_path}</p>
              <p className="nums mt-0.5 flex flex-wrap gap-x-2 text-[13px]" style={{ color: "var(--text-muted)" }}>
                <span>{bytes(file.length)}</span>
                <span aria-hidden="true">·</span>
                <span title={meter.hint}>
                  <span aria-hidden="true">{meter.meter}</span> {meter.label}
                </span>
                {file.materialized_as && (
                  <>
                    <span aria-hidden="true">·</span>
                    <span>{file.materialized_as}</span>
                  </>
                )}
              </p>
              {file.source ? (
                <p className="mt-1 text-[13px] break-path" style={{ color: "var(--text-muted)" }}>
                  ← {file.source}
                </p>
              ) : (
                <p className="mt-1 text-[13px]" style={{ color: "var(--state-review)" }}>
                  No library file chosen yet
                </p>
              )}
              {file.recheck_progress !== null && (
                <div className="mt-2">
                  <Progress
                    value={file.recheck_progress}
                    label={`Checked ${file.torrent_path}`}
                    tone={file.recheck_progress >= 1 ? "completed" : "rechecking"}
                  />
                </div>
              )}
            </Card>
          );
        })}
      </ul>

      <div className="scroll-x hidden md:block" data-allow-xscroll>
        <table className="w-full text-[13px]">
          <thead>
            <tr className="text-left" style={{ color: "var(--text-muted)" }}>
              <th className="py-1.5 pr-3 font-medium">File</th>
              <th className="py-1.5 pr-3 font-medium">Size</th>
              <th className="py-1.5 pr-3 font-medium">Confidence</th>
              <th className="py-1.5 pr-3 font-medium">From</th>
              <th className="py-1.5 font-medium">Checked</th>
            </tr>
          </thead>
          <tbody>
            {files.map((file) => {
              const meter = confidenceMeter(file.confidence);
              return (
                <tr key={file.torrent_path} className="border-t" style={{ borderColor: "var(--border)" }}>
                  <td className="py-2 pr-3 break-path">{file.torrent_path}</td>
                  <td className="py-2 pr-3">{bytes(file.length)}</td>
                  <td className="py-2 pr-3" title={meter.hint}>
                    <span aria-hidden="true">{meter.meter}</span> {meter.label}
                  </td>
                  <td className="py-2 pr-3 break-path">
                    {file.source ?? <span style={{ color: "var(--state-review)" }}>not chosen</span>}
                  </td>
                  <td className="py-2">
                    {file.recheck_progress === null
                      ? "—"
                      : file.recheck_progress >= 1
                        ? "complete"
                        : `${(file.recheck_progress * 100).toFixed(1)}%`}
                  </td>
                </tr>
              );
            })}
          </tbody>
        </table>
      </div>
    </>
  );
}

// --- the candidate flow ---------------------------------------------------

/**
 * One decision at a time, with the evidence behind each option.
 *
 * The old UI was a bare `<select>` per unmatched file with no context at all. The
 * choice is still sent as `{torrent_path, candidate_index}` — an index into the
 * server's own list, never a path, because a torrent-supplied path is hostile
 * input.
 */
function CandidatePicker({
  detail,
  onChose,
  onError,
  onClose,
  open,
}: {
  detail: JobDetail;
  onChose: (resolved: boolean) => void;
  onError: (error: unknown) => void;
  onClose: () => void;
  open: boolean;
}) {
  const undecided = detail.files.filter((file) => file.source === null && file.candidates.length > 0);
  const [step, setStep] = useState(0);
  const [choice, setChoice] = useState<number | null>(null);
  const [busy, setBusy] = useState(false);
  const headingRef = useRef<HTMLParagraphElement>(null);

  const file = undecided[step];

  // Focus the step heading, never a radio — landing on a radio reads the first
  // candidate as if it were already selected.
  useEffect(() => {
    if (open) headingRef.current?.focus();
    setChoice(null);
  }, [step, open]);

  if (!open) return null;

  if (!file) {
    return (
      <Dialog open onClose={onClose} title="Nothing left to choose">
        <p>Every file in this repair already has a library file.</p>
      </Dialog>
    );
  }

  const submit = async () => {
    if (choice === null) return;
    setBusy(true);
    try {
      const response = await api.act(detail.job.id, ACTION_PATH.choose_candidate, {
        torrent_path: file.torrent_path,
        candidate_index: choice,
      });
      const resolved = response.resolved === true;
      if (resolved || step + 1 >= undecided.length) {
        onChose(resolved);
      } else {
        setStep((current) => current + 1);
      }
    } catch (error) {
      onError(error);
    } finally {
      setBusy(false);
    }
  };

  return (
    <Dialog
      open
      onClose={onClose}
      title="Choose a library file"
      footer={
        <Button variant="primary" disabled={choice === null || busy} onClick={() => void submit()}>
          {busy ? "Saving…" : "Use this file"}
        </Button>
      }
    >
      <div>
        <Progress value={step} max={Math.max(1, undecided.length)} label="Decision progress" />
        <p className="nums mt-1 text-[13px]" style={{ color: "var(--text-muted)" }}>
          File {step + 1} of {undecided.length}
        </p>
      </div>

      <p ref={headingRef} tabIndex={-1} className="font-medium break-path">
        The torrent needs {file.torrent_path}
      </p>
      <p className="nums text-[13px]" style={{ color: "var(--text-muted)" }}>
        {bytes(file.length)} — {file.candidates.length} library file
        {file.candidates.length === 1 ? " is" : "s are"} exactly that size, so matching stopped and
        asked.
      </p>

      <fieldset className="space-y-2">
        <legend className="sr-only">Candidates for {file.torrent_path}</legend>
        {file.candidates.map((candidate) => {
          const evidence = file.evidence;
          return (
            <label
              key={candidate.index}
              className="flex cursor-pointer items-start gap-3 rounded-[var(--radius-md)] border p-3"
              style={{
                borderColor: choice === candidate.index ? "var(--accent-on-soft)" : "var(--border)",
                background: choice === candidate.index ? "var(--accent-soft)" : "transparent",
              }}
            >
              <input
                type="radio"
                name="candidate"
                className="mt-1 size-5 shrink-0"
                checked={choice === candidate.index}
                onChange={() => setChoice(candidate.index)}
              />
              <span className="min-w-0 flex-1">
                <span className="block break-path">{candidate.path}</span>
                <span className="mt-0.5 block text-[13px]" style={{ color: "var(--text-muted)" }}>
                  {candidate.origin.kind === "filesystem"
                    ? `filesystem · ${candidate.origin.root}`
                    : `${candidate.origin.kind} · ${candidate.origin.instance}`}
                </span>
                {/* The evidence that existed in the database and was rendered
                    nowhere. Ticks and crosses plus words, never colour alone. */}
                <span className="mt-1 flex flex-wrap gap-x-3 text-[12px]" style={{ color: "var(--text-muted)" }}>
                  <span>size {evidence?.size_matches ? "✓ matches" : "✕ differs"}</span>
                  <span>name {evidence?.name_matches ? "✓ matches" : "✕ differs"}</span>
                  <span>pieces {evidence?.piece_verified ? "✓ verified" : "— not checked"}</span>
                </span>
              </span>
            </label>
          );
        })}
      </fieldset>

      <Banner tone="info">
        Choosing marks this file as picked by you. Size agreement alone is evidence, not proof — the
        repair still gets a full recheck before anything is seeded.
      </Banner>
    </Dialog>
  );
}

// --- the screen -----------------------------------------------------------

export function JobScreen({
  id,
  notify,
  onError,
  live,
}: {
  id: number;
  notify: (tone: "success" | "danger" | "info", message: string, detail?: string) => void;
  onError: (error: unknown) => void;
  live: LiveStatus;
}) {
  const [detail, setDetail] = useState<JobDetail | null>(null);
  const [loading, setLoading] = useState(true);
  const [pending, setPending] = useState<string | null>(null);
  const [confirming, setConfirming] = useState<ActionName | null>(null);
  const [picking, setPicking] = useState(false);
  const [acknowledged, setAcknowledged] = useState(false);

  const load = useCallback(async () => {
    try {
      setDetail(await api.job(id));
    } catch (error) {
      onError(error);
    } finally {
      setLoading(false);
    }
  }, [id, onError]);

  useEffect(() => {
    void load();
  }, [load]);
  useEffect(() => {
    if (live === "live") void load();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [live]);

  const run = async (action: ActionName) => {
    setConfirming(null);
    setAcknowledged(false);
    setPending(action);
    try {
      const path = ACTION_PATH[action];
      await api.act(id, path);
      const next = await api.job(id);
      setDetail(next);
      notify("success", SUCCESS[path]?.(next) ?? "Done.");
    } catch (error) {
      onError(error);
      // Resync: a 409 means the job moved, so the panel must reflect reality
      // before the operator tries again.
      void load();
    } finally {
      setPending(null);
    }
  };

  if (loading && !detail) return <Skeleton rows={5} />;
  if (!detail) {
    return (
      <Card>
        <EmptyState glyph="🔍" title="No such repair">
          <Link to="/repairs">Back to all repairs</Link>
        </EmptyState>
      </Card>
    );
  }

  const { job, actions, files, history } = detail;
  const due = deadline(job.deadline);
  const parked = job.state === "awaiting_review";
  const destructive: ActionName[] = ["restart", "abandon", "abandon_and_discard", "discard_staging"];

  const label: Record<ActionName, string> = {
    retry: actions.retry.resume_to ? `Retry from ${stateLabel(actions.retry.resume_to).toLowerCase()}` : "Retry",
    restart: "Start over",
    abandon: "Abandon",
    abandon_and_discard: "Abandon and discard staged files",
    approve_resume: "Approve resume",
    choose_candidate: "Choose files",
    discard_staging: "Discard staged files",
  };

  const order: ActionName[] = [
    "approve_resume",
    "choose_candidate",
    "retry",
    "restart",
    "abandon",
    "abandon_and_discard",
    "discard_staging",
  ];

  return (
    <>
      <p className="mb-2 text-[13px]">
        <Link to="/repairs">← All repairs</Link>
      </p>

      <div className="mb-1 flex flex-wrap items-center gap-2">
        <StateChip state={job.state} />
        {due && (
          <Badge tone={due.urgency === "past" ? "failed" : due.urgency === "soon" ? "review" : "neutral"}>
            {due.text}
          </Badge>
        )}
        {job.resume_approved && <Badge tone="accent">resume approved</Badge>}
      </div>
      <h1 className="text-[21px] leading-tight font-semibold break-path">{job.torrent_name}</h1>
      {/* The decision panel repeats the review reason verbatim, so printing it
          here too is the same sentence twice on one screen. */}
      {!parked && (
        <p className="mt-1 text-[14px] break-path" style={{ color: "var(--text-muted)" }}>
          {job.explain}
        </p>
      )}

      <div className="mt-4">
        <Stepper rank={job.state_rank} total={job.state_total} state={job.state} />
      </div>

      {/* The decision panel, immediately after the heading — closest thing on the
          page to "what do you want me to do". */}
      {(parked || job.state === "failed" || actions.discard_staging.available) && (
        <Card className="mt-4">
          <SectionHeading>{parked ? "This repair needs a decision" : "Actions"}</SectionHeading>
          {job.review_reason_description && (
            <p className="mb-3 text-[14px]">{job.review_reason_description}</p>
          )}
          {job.failure_reason && job.state === "failed" && (
            <p className="mb-3 text-[14px]" style={{ color: "var(--state-failed)" }}>
              {job.failure_reason}
            </p>
          )}
          <div className="flex flex-wrap gap-2">
            {order.map((action) => {
              const state = actions[action];
              if (!state.available) return null;
              if (action === "choose_candidate") {
                return (
                  <Button key={action} variant="primary" onClick={() => setPicking(true)}>
                    {label[action]}
                    {state.unresolved_files ? ` (${state.unresolved_files})` : ""}
                  </Button>
                );
              }
              return (
                <Button
                  key={action}
                  variant={
                    destructive.includes(action)
                      ? "danger"
                      : action === "approve_resume" || action === "retry"
                        ? "primary"
                        : "secondary"
                  }
                  disabled={pending !== null}
                  onClick={() =>
                    destructive.includes(action) ? setConfirming(action) : void run(action)
                  }
                >
                  {pending === action ? "Working…" : label[action]}
                </Button>
              );
            })}
          </div>

          {/* Why an action is missing, in the server's words — so a disabled or
              absent control is never a mystery. */}
          <details className="mt-3">
            <summary className="inline-flex min-h-11 cursor-pointer items-center text-[13px]" style={{ color: "var(--text-muted)" }}>
              Why can I not do more?
            </summary>
            <ul className="mt-1 space-y-1 text-[13px]" style={{ color: "var(--text-muted)" }}>
              {order
                .filter((action) => !actions[action].available && actions[action].why)
                .map((action) => (
                  <li key={action} className="break-path">
                    <strong>{label[action]}:</strong> {actions[action].why}
                  </li>
                ))}
            </ul>
          </details>
        </Card>
      )}

      {job.state === "rechecking" && (
        <Card className="mt-4">
          <SectionHeading>Checking</SectionHeading>
          <Progress value={null} label="Checking staged data" tone="rechecking" />
          <p className="mt-2 text-[14px]" style={{ color: "var(--text-muted)" }}>
            Started {relative(job.rechecking_started_at)}. The download client is verifying every
            piece; SeedMedic waits rather than assuming.
          </p>
        </Card>
      )}

      {job.state === "seeding" && (
        <Card className="mt-4">
          <SectionHeading>Seeding</SectionHeading>
          <dl className="grid grid-cols-[max-content_1fr] gap-x-4 gap-y-1 text-[14px]">
            <dt style={{ color: "var(--text-muted)" }}>Uploaded</dt>
            <dd className="nums">{bytes(job.uploaded_bytes)}</dd>
            <dt style={{ color: "var(--text-muted)" }}>Seeding for</dt>
            <dd className="nums">{duration(job.seeding_seconds)}</dd>
          </dl>
          <p className="mt-2 text-[14px]" style={{ color: "var(--text-muted)" }}>
            These are the download client's numbers. The repair is finished when the{" "}
            <strong>tracker</strong> says the hit-and-run is cleared, which can lag behind.
          </p>
        </Card>
      )}

      <section className="mt-5">
        <SectionHeading>Details</SectionHeading>
        <Card>
          <dl className="grid grid-cols-[max-content_1fr] gap-x-4 gap-y-1.5 text-[14px]">
            <dt style={{ color: "var(--text-muted)" }}>Tracker</dt>
            <dd className="break-path">
              {job.tracker} / {job.torrent_id}
            </dd>
            <dt style={{ color: "var(--text-muted)" }}>Size</dt>
            <dd className="nums">{bytes(job.total_bytes)}</dd>
            {detail.staged_bytes !== null && (
              <>
                <dt style={{ color: "var(--text-muted)" }}>Staged on disk</dt>
                <dd className="nums">{bytes(detail.staged_bytes)}</dd>
              </>
            )}
            {job.materialization && (
              <>
                <dt style={{ color: "var(--text-muted)" }}>Materialised by</dt>
                <dd>
                  {job.materialization}
                  {job.materialization === "hardlink" && (
                    <span style={{ color: "var(--state-review)" }}>
                      {" "}
                      — shares inodes with your library
                    </span>
                  )}
                </dd>
              </>
            )}
            {job.info_hash && (
              <>
                <dt style={{ color: "var(--text-muted)" }}>Info-hash</dt>
                <dd className="break-path font-mono text-[12px]">{middleTruncate(job.info_hash, 44)}</dd>
              </>
            )}
            <dt style={{ color: "var(--text-muted)" }}>Attempts</dt>
            <dd className="nums">
              {job.attempts}
              {job.next_attempt_at && ` · next ${relative(job.next_attempt_at)}`}
            </dd>
            <dt style={{ color: "var(--text-muted)" }}>Discovered</dt>
            <dd>
              <time dateTime={job.created_at} title={absolute(job.created_at)}>
                {relative(job.created_at)}
              </time>
            </dd>
          </dl>
        </Card>
      </section>

      {files.length > 0 && (
        <section className="mt-5">
          <SectionHeading>Files</SectionHeading>
          <FilePlan files={files} />
        </section>
      )}

      <section className="mt-5">
        <SectionHeading>History</SectionHeading>
        <Card>
          {history.length === 0 ? (
            <p style={{ color: "var(--text-muted)" }}>Nothing recorded yet.</p>
          ) : (
            <Timeline history={history} />
          )}
        </Card>
      </section>

      <CandidatePicker
        open={picking}
        detail={detail}
        onClose={() => setPicking(false)}
        onError={onError}
        onChose={(resolved) => {
          setPicking(false);
          void load();
          notify(
            "success",
            resolved
              ? "Every file has a library file now — the repair is on its way to staging."
              : "Choice saved. This repair is still waiting on the remaining files.",
          );
        }}
      />

      <Dialog
        open={confirming !== null}
        onClose={() => {
          setConfirming(null);
          setAcknowledged(false);
        }}
        destructive
        title={confirming ? label[confirming] : ""}
        footer={
          <Button
            variant="danger"
            disabled={!acknowledged}
            onClick={() => confirming && void run(confirming)}
          >
            {confirming ? label[confirming] : ""}
          </Button>
        }
      >
        {confirming === "abandon" && (
          <p>
            This repair will be marked failed. Staged files are <strong>not</strong> deleted.
          </p>
        )}
        {confirming === "restart" && (
          <p>
            The repair goes back to the beginning: the torrent is removed from the download client
            and everything under{" "}
            <code className="break-path">{job.staging_dir ?? "its staging directory"}</code> is
            deleted. Your library is never touched.
          </p>
        )}
        {confirming === "abandon_and_discard" && (
          <p>
            The repair is marked failed, the torrent is removed from the download client, and{" "}
            <strong>{bytes(detail.staged_bytes)}</strong> under{" "}
            <code className="break-path">{job.staging_dir}</code> is deleted. Your library is never
            touched.
          </p>
        )}
        {confirming === "discard_staging" && (
          <p>
            The tracker has cleared this hit-and-run, so the staged copy has done its job.{" "}
            <strong>{bytes(detail.staged_bytes)}</strong> will be freed and{" "}
            <strong>this torrent will stop seeding</strong>.
          </p>
        )}
        <label className="flex items-start gap-2.5">
          <input
            type="checkbox"
            className="mt-0.5 size-5 shrink-0"
            checked={acknowledged}
            onChange={(event) => setAcknowledged(event.target.checked)}
          />
          <span>I understand this cannot be undone.</span>
        </label>
      </Dialog>
    </>
  );
}
