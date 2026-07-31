/** Presentation helpers. Formatting is the client's job; rules are the server's. */

import type { MatchConfidence, RepairState } from "./api";

const UNITS = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"] as const;

export function bytes(value: number | null | undefined): string {
  if (value === null || value === undefined) return "—";
  let n = value;
  let unit = 0;
  while (n >= 1024 && unit < UNITS.length - 1) {
    n /= 1024;
    unit += 1;
  }
  return unit === 0 ? `${value} B` : `${n.toFixed(1)} ${UNITS[unit]}`;
}

const RELATIVE = new Intl.RelativeTimeFormat(undefined, { numeric: "auto" });

/**
 * "4 minutes ago". `Intl` rather than a date library: this and `absolute` are the
 * only two date formats the app needs.
 */
export function relative(iso: string | null | undefined, now = Date.now()): string {
  if (!iso) return "never";
  const then = Date.parse(iso);
  if (Number.isNaN(then)) return "unknown";

  const seconds = Math.round((then - now) / 1000);
  const magnitude = Math.abs(seconds);
  if (magnitude < 45) return "just now";
  if (magnitude < 3600) return RELATIVE.format(Math.round(seconds / 60), "minute");
  if (magnitude < 86_400) return RELATIVE.format(Math.round(seconds / 3600), "hour");
  return RELATIVE.format(Math.round(seconds / 86_400), "day");
}

/** The exact timestamp, for a `title` and a `datetime` attribute. */
export function absolute(iso: string | null | undefined): string {
  if (!iso) return "";
  const date = new Date(iso);
  return Number.isNaN(date.getTime()) ? "" : date.toLocaleString();
}

/**
 * How much time is left before a hit-and-run becomes a penalty — the only
 * urgency signal in the whole system.
 *
 * `null` when the tracker did not say, which is the common case for the demo
 * adapters. Every deadline affordance must degrade to *absent* rather than to a
 * dash, because "no deadline" and "deadline unknown" are the same thing here and
 * neither is "due now".
 */
export function deadline(
  iso: string | null | undefined,
  now = Date.now(),
): { text: string; urgency: "past" | "soon" | "later" } | null {
  if (!iso) return null;
  const at = Date.parse(iso);
  if (Number.isNaN(at)) return null;

  const hours = (at - now) / 3_600_000;
  if (hours < 0) return { text: "deadline passed", urgency: "past" };
  if (hours < 24) return { text: `${Math.max(1, Math.round(hours))}h to deadline`, urgency: "soon" };
  return { text: `${Math.round(hours / 24)}d to deadline`, urgency: "later" };
}

export function duration(seconds: number | null | undefined): string {
  if (seconds === null || seconds === undefined) return "—";
  if (seconds < 60) return `${seconds}s`;
  if (seconds < 3600) return `${Math.round(seconds / 60)}m`;
  if (seconds < 86_400) return `${(seconds / 3600).toFixed(1)}h`;
  return `${(seconds / 86_400).toFixed(1)}d`;
}

/** Turn `awaiting_review` into `Awaiting review`. */
export function stateLabel(state: RepairState): string {
  const words = state.replace(/_/g, " ");
  return words.charAt(0).toUpperCase() + words.slice(1);
}

/**
 * The visual family a state belongs to.
 *
 * Exhaustive on purpose: the `never` fallthrough means adding a `RepairState` in
 * Rust and regenerating the union fails `tsc` here rather than rendering an
 * unstyled chip.
 */
export function stateTone(state: RepairState): string {
  switch (state) {
    case "discovered":
    case "torrent_fetched":
    case "matched":
    case "staged":
    case "injected":
      return "neutral";
    case "rechecking":
      return "rechecking";
    case "verified":
      return "verified";
    case "seeding":
      return "seeding";
    case "completed":
      return "completed";
    case "awaiting_review":
      return "review";
    case "failed":
      return "failed";
    default: {
      const exhaustive: never = state;
      throw new Error(`unhandled repair state: ${String(exhaustive)}`);
    }
  }
}

/**
 * A glyph per state, so colour is never the only signal.
 *
 * `completed` and `failed` deliberately differ in *shape* (✓ against ✕) rather
 * than only in hue, so they stay distinguishable in greyscale and to anyone with
 * a red/green deficiency.
 */
export function stateGlyph(state: RepairState): string {
  switch (stateTone(state)) {
    case "completed":
      return "✓";
    case "failed":
      return "✕";
    case "review":
      return "▲";
    case "seeding":
      return "↑";
    case "rechecking":
      return "◐";
    case "verified":
      return "◆";
    default:
      return "●";
  }
}

/** A four-step meter plus a word — again, never colour alone. */
export function confidenceMeter(confidence: MatchConfidence | null): {
  meter: string;
  label: string;
  hint: string;
} {
  switch (confidence) {
    case "exact":
      return { meter: "▇▇▇▇", label: "Exact", hint: "Verified against the torrent's piece hashes." };
    case "operator":
      return { meter: "▇▇▇▁", label: "Chosen", hint: "You picked this file." };
    case "probable":
      return { meter: "▇▇▁▁", label: "Probable", hint: "Size and name both agree." };
    case "ambiguous":
      return {
        meter: "▇▁▁▁",
        label: "Ambiguous",
        hint: "Only the size agrees, or several files matched — size alone is evidence, not proof.",
      };
    case null:
      return { meter: "▁▁▁▁", label: "Unmatched", hint: "No library file chosen yet." };
    default: {
      const exhaustive: never = confidence;
      throw new Error(`unhandled confidence: ${String(exhaustive)}`);
    }
  }
}

/** Middle-truncate a long path so both ends stay readable in a narrow column. */
export function middleTruncate(text: string, max = 48): string {
  if (text.length <= max) return text;
  const half = Math.floor((max - 1) / 2);
  return `${text.slice(0, half)}…${text.slice(-half)}`;
}
