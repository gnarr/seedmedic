/**
 * The `/api/v1` client, and the types it returns.
 *
 * Types are hand-written and kept honest by `contract.test.ts`, which asserts
 * every union against a fixture the Rust side generates. The unions that matter —
 * `RepairState`, `ReviewReason`, `MatchConfidence` — are exhaustively switched on
 * with a `never` assertion, so adding a variant in Rust fails the TypeScript
 * build rather than rendering blank.
 */

// --- lifecycle ------------------------------------------------------------

/** The nine happy-path states, in order, plus the two exits. */
export const PROGRESSION = [
  "discovered",
  "torrent_fetched",
  "matched",
  "staged",
  "injected",
  "rechecking",
  "verified",
  "seeding",
  "completed",
] as const;

export type RepairState = (typeof PROGRESSION)[number] | "awaiting_review" | "failed";

export type MatchConfidence = "ambiguous" | "probable" | "operator" | "exact";

export type CandidateOrigin =
  | { kind: "sonarr"; instance: string }
  | { kind: "radarr"; instance: string }
  | { kind: "filesystem"; root: string };

export interface MatchEvidence {
  size_matches: boolean;
  name_matches: boolean;
  candidates_with_matching_size: number;
  piece_verified: boolean;
}

export interface Job {
  id: number;
  tracker: string;
  torrent_id: string;
  torrent_name: string;
  state: RepairState;
  review_from_state: RepairState | null;
  review_reason: string | null;
  failure_reason: string | null;
  info_hash: string | null;
  total_bytes: number | null;
  staging_dir: string | null;
  materialization: "reflink" | "hardlink" | "copy" | null;
  deadline: string | null;
  uploaded_bytes: number | null;
  seeding_seconds: number | null;
  rechecking_started_at: string | null;
  consecutive_unknown_tracker_status: number;
  resume_approved: boolean;
  attempts: number;
  next_attempt_at: string | null;
  created_at: string;
  updated_at: string;
  /** Position on the happy path, or null off it. */
  state_rank: number | null;
  state_total: number;
  is_terminal: boolean;
  is_actionable: boolean;
  /** One line saying where this stands, composed by the server. */
  explain: string;
  review_reason_description: string | null;
}

export interface CandidateChoice {
  /** What the server resolves the choice by. Never send a path. */
  index: number;
  path: string;
  origin: CandidateOrigin;
}

export interface PlannedFile {
  torrent_path: string;
  length: number;
  source: string | null;
  confidence: MatchConfidence | null;
  evidence: MatchEvidence | null;
  materialized_as: "reflink" | "hardlink" | "copy" | null;
  recheck_progress: number | null;
  candidates: CandidateChoice[];
}

export interface Transition {
  from: RepairState;
  to: RepairState;
  reason: string;
  detail: Record<string, unknown> | null;
  occurred_at: string;
}

export interface ActionState {
  available: boolean;
  why?: string;
  resume_to?: RepairState;
  unresolved_files?: number;
}

export type ActionName =
  | "retry"
  | "restart"
  | "abandon"
  | "abandon_and_discard"
  | "approve_resume"
  | "choose_candidate"
  | "discard_staging";

export type Actions = Record<ActionName, ActionState>;

export interface JobDetail {
  job: Job;
  files: PlannedFile[];
  history: Transition[];
  staged_bytes: number | null;
  actions: Actions;
}

export interface JobList {
  jobs: Job[];
  next_cursor: string | null;
  total_matching: number;
}

export interface Dashboard {
  generated_at: string;
  counts: {
    total: number;
    by_state: { state: RepairState; count: number }[];
    by_review_reason: { reason: string | null; description: string | null; count: number }[];
  };
  attention: {
    review: number;
    failed: number;
    stuck: { job: number; torrent_name: string; reason: string; detail: string }[];
  };
  worker: {
    last_tick: string | null;
    stale: boolean;
    threshold_seconds: number;
    last_tick_summary: ActivitySummary | null;
    last_discovery: ActivitySummary | null;
    last_reconcile: ActivitySummary | null;
  };
  trackers: {
    id: string;
    adapter: string;
    stub: boolean;
    last_success: string | null;
    last_error: { at: string; message: string } | null;
    unfinished_jobs: number;
  }[];
  setup: {
    config_path: string;
    warnings: string[];
    /** Three states, never a boolean. `unknown` means the page cannot tell. */
    auth: "set" | "unset" | "unknown";
  };
}

export interface ActivitySummary {
  at: string | null;
  claimed: number;
  advanced: number;
  parked: number;
  retrying: number;
  rewound: number;
  jobs_created: number;
  trackers_failed: number;
}

export interface Diagnostics {
  generated_at: string;
  download_client: {
    adapter: string;
    stub: boolean;
    reachable: boolean;
    torrent_count: number | null;
    error: string | null;
  };
  staging: {
    configured: boolean;
    root: string | null;
    free_bytes: number | null;
    held_bytes: number;
    declared_bytes: number;
  };
  policy_summary: string;
  ready: boolean;
}

export interface Session {
  auth: "set" | "unset" | "unknown";
  authenticated: boolean;
  app: { version: string; features: { fakes: boolean; metrics: boolean } };
}

export interface BulkResponse {
  action: string;
  applied: number;
  total: number;
  results: { id: number; ok: boolean; message?: string }[];
}

// --- errors ---------------------------------------------------------------

/** The server's own refusal, kept verbatim: its prose is better than ours. */
export class ApiError extends Error {
  readonly status: number;
  readonly code: string;
  readonly fields: Record<string, string>;
  readonly general: string[];

  constructor(
    status: number,
    code: string,
    message: string,
    fields: Record<string, string> = {},
    general: string[] = [],
  ) {
    super(message);
    this.status = status;
    this.code = code;
    this.fields = fields;
    this.general = general;
  }

  /** A refused state transition — the remedy is refetch, not retry. */
  get isConflict() {
    return this.status === 409;
  }
}

export class Unauthorized extends Error {}

// --- transport ------------------------------------------------------------

/**
 * `<base href>` is injected by the server from `server.base_path`, so one bundle
 * works at `/` and behind a reverse proxy at `/seedmedic/` without a rebuild.
 */
function apiRoot(): string {
  const base = document.baseURI.replace(/\/$/, "");
  return `${base}/api/v1`;
}

async function request<T>(path: string, init?: RequestInit): Promise<T> {
  const response = await fetch(`${apiRoot()}${path}`, {
    ...init,
    // The session cookie. `SameSite=Strict` is the CSRF control, so nothing is
    // sent cross-site in the first place.
    credentials: "same-origin",
    headers: {
      Accept: "application/json",
      ...(init?.body ? { "Content-Type": "application/json" } : {}),
      ...init?.headers,
    },
  });

  if (response.status === 401) throw new Unauthorized();
  if (response.status === 204) return undefined as T;

  const body: unknown = await response.json().catch(() => null);
  if (!response.ok) {
    const error =
      body && typeof body === "object" && "error" in body
        ? (body as { error: { code: string; message: string; fields?: Record<string, string>; general?: string[] } })
            .error
        : null;
    throw new ApiError(
      response.status,
      error?.code ?? "unknown",
      error?.message ?? response.statusText,
      error?.fields ?? {},
      error?.general ?? [],
    );
  }
  return body as T;
}

export const api = {
  session: () => request<Session>("/session"),
  signIn: (token: string) =>
    request<void>("/session", { method: "POST", body: JSON.stringify({ token }) }),
  signOut: () => request<void>("/session", { method: "DELETE" }),

  dashboard: () => request<Dashboard>("/dashboard"),
  diagnostics: () => request<Diagnostics>("/diagnostics"),

  jobs: (params: Record<string, string | string[] | undefined>) => {
    const query = new URLSearchParams();
    for (const [key, value] of Object.entries(params)) {
      if (value === undefined) continue;
      // Repeatable keys are appended, not joined: the server decodes a pair
      // list, so `state=a&state=b` is two values.
      for (const one of Array.isArray(value) ? value : [value]) {
        if (one) query.append(key, one);
      }
    }
    const suffix = query.toString();
    return request<JobList>(`/jobs${suffix ? `?${suffix}` : ""}`);
  },

  job: (id: number) => request<JobDetail>(`/jobs/${id}`),

  act: (id: number, action: string, body?: unknown) =>
    request<{ job: Job; actions: Actions; resolved?: boolean }>(`/jobs/${id}/${action}`, {
      method: "POST",
      ...(body ? { body: JSON.stringify(body) } : {}),
    }),

  bulk: (action: "retry" | "abandon", ids: number[]) =>
    request<BulkResponse>(`/jobs/bulk/${action}`, {
      method: "POST",
      body: JSON.stringify({ ids }),
    }),

  eventsUrl: () => `${apiRoot()}/events`,
};

/** The action names as the URL spells them. */
export const ACTION_PATH: Record<ActionName, string> = {
  retry: "retry",
  restart: "restart",
  abandon: "abandon",
  abandon_and_discard: "abandon-and-discard",
  approve_resume: "approve-resume",
  choose_candidate: "choose-candidate",
  discard_staging: "discard-staging",
};
