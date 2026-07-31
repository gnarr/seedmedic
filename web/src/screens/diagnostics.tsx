/**
 * Diagnostics: the detail behind the dashboard's integration pills.
 *
 * Its own screen because it costs real work — it probes the download client and
 * walks the staging filesystem once per job. The dashboard must stay cheap enough
 * to refetch on every event, so this is a page you open rather than a panel that
 * refreshes.
 */

import { useEffect, useState } from "react";
import { api, type Diagnostics } from "../api";
import { bytes, relative } from "../format";
import { Banner, Button, Card, SectionHeading, Skeleton } from "../ui";

export function DiagnosticsScreen({ onError }: { onError: (error: unknown) => void }) {
  const [data, setData] = useState<Diagnostics | null>(null);
  const [loading, setLoading] = useState(true);

  const load = () => {
    setLoading(true);
    void api
      .diagnostics()
      .then(setData)
      .catch(onError)
      .finally(() => setLoading(false));
  };

  useEffect(load, []);

  if (loading && !data) {
    return (
      <>
        <h1 className="mb-3 text-[22px] font-semibold tracking-tight">Diagnostics</h1>
        <Skeleton rows={3} />
      </>
    );
  }
  if (!data) return null;

  const { download_client: client, staging } = data;
  const drift = staging.held_bytes !== staging.declared_bytes;

  return (
    <>
      <div className="mb-3 flex flex-wrap items-baseline justify-between gap-2">
        <h1 className="text-[22px] font-semibold tracking-tight">Diagnostics</h1>
        <p className="text-[13px]" style={{ color: "var(--text-muted)" }}>
          measured {relative(data.generated_at)}
        </p>
      </div>

      {!data.ready && (
        <div className="mb-4">
          <Banner tone="danger" title="Not ready">
            The database did not answer. Nothing can advance until it does.
          </Banner>
        </div>
      )}

      <section className="mb-5">
        <SectionHeading>Download client</SectionHeading>
        <Card>
          <div className="mb-2">
            {client.reachable ? (
              <Banner tone="success">
                Reachable — holding {client.torrent_count ?? 0} torrent
                {client.torrent_count === 1 ? "" : "s"}.
              </Banner>
            ) : (
              <Banner tone="danger" title="Not reachable">
                {client.error ?? "No reason given."}
              </Banner>
            )}
          </div>
          <dl className="grid grid-cols-[max-content_1fr] gap-x-4 gap-y-1 text-[14px]">
            <dt style={{ color: "var(--text-muted)" }}>Adapter</dt>
            <dd>
              {client.adapter}
              {client.stub && " (demo)"}
            </dd>
          </dl>
        </Card>
      </section>

      <section className="mb-5">
        <SectionHeading>Staging</SectionHeading>
        <Card>
          {staging.configured ? (
            <dl className="grid grid-cols-[max-content_1fr] gap-x-4 gap-y-1.5 text-[14px]">
              <dt style={{ color: "var(--text-muted)" }}>Root</dt>
              <dd className="break-path">{staging.root}</dd>
              <dt style={{ color: "var(--text-muted)" }}>Free space</dt>
              <dd className="nums">{bytes(staging.free_bytes)}</dd>
              <dt style={{ color: "var(--text-muted)" }}>On disk</dt>
              <dd className="nums">{bytes(staging.held_bytes)}</dd>
              <dt style={{ color: "var(--text-muted)" }}>Expected</dt>
              <dd className="nums">{bytes(staging.declared_bytes)}</dd>
            </dl>
          ) : (
            <Banner tone="warning" title="Staging is not configured">
              No repair can be materialised until <code>staging.root</code> is set.
            </Banner>
          )}
          {staging.configured && drift && (
            <div className="mt-3">
              {/* Two numbers that disagree is itself a signal, so both are shown
                  rather than one being quietly preferred. */}
              <Banner tone="info">
                What is on disk differs from what SeedMedic recorded. That is normal shortly after a
                discard, and worth a look otherwise.
              </Banner>
            </div>
          )}
        </Card>
      </section>

      <section>
        <SectionHeading>Effective configuration</SectionHeading>
        <Card>
          <p className="mb-2 text-[13px]" style={{ color: "var(--text-muted)" }}>
            Every secret is reduced to <code>set</code> or <code>unset</code> — this is safe to paste
            into a bug report.
          </p>
          <pre
            className="scroll-x rounded-[var(--radius-sm)] p-3 text-[12px]"
            style={{ background: "var(--surface-2)" }}
            data-allow-xscroll
          >
            {data.policy_summary}
          </pre>
        </Card>
      </section>

      <div className="mt-4 flex justify-end">
        <Button onClick={load}>Re-measure</Button>
      </div>
    </>
  );
}
