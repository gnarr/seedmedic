/**
 * The shell: navigation, global banners, live-connection state, routing.
 *
 * Navigation is the single biggest defect this rebuild fixes. The old header was a
 * wordmark, a tagline and a sign-out button — `/status` and `/settings` could only
 * be reached by typing the URL.
 *
 * Four destinations on mobile, as a bottom tab bar; the same four as a sidebar
 * from 1024px up. **Review is a tab rather than a filter** because the product's
 * thesis is that a repair which stops and asks a human is a success, so the queue
 * that needs a person is a place, not a view option. Diagnostics is deliberately
 * *not* a tab: the dashboard carries the signals, and the detail lives one click
 * behind them.
 */

import { useCallback, useEffect, useRef, useState } from "react";
import { ApiError, Unauthorized, api, type Dashboard, type Session } from "./api";
import { Badge, Banner, Button, IconButton, LinkButton, LiveRegion, ToastHost, useToasts } from "./ui";
import { relative } from "./format";
import { DashboardScreen } from "./screens/dashboard";
import { RepairsScreen } from "./screens/repairs";
import { JobScreen } from "./screens/job";
import { DiagnosticsScreen } from "./screens/diagnostics";
import { LoginScreen } from "./screens/login";

// --- routing --------------------------------------------------------------

export type Route =
  | { name: "dashboard" }
  | { name: "repairs"; query: URLSearchParams }
  | { name: "review" }
  | { name: "job"; id: number }
  | { name: "diagnostics" }
  | { name: "login" }
  | { name: "notFound"; path: string };

/** The app's base path, from the `<base href>` the server injects. */
function basePath(): string {
  return new URL(document.baseURI).pathname.replace(/\/$/, "");
}

function parse(pathname: string, search: string): Route {
  const path = pathname.slice(basePath().length) || "/";
  const query = new URLSearchParams(search);

  if (path === "/" || path === "") return { name: "dashboard" };
  if (path === "/repairs") return { name: "repairs", query };
  if (path === "/review") return { name: "review" };
  if (path === "/diagnostics") return { name: "diagnostics" };
  if (path === "/login") return { name: "login" };

  const job = /^\/repairs\/(\d+)$/.exec(path);
  if (job?.[1]) return { name: "job", id: Number(job[1]) };

  return { name: "notFound", path };
}

export function navigate(to: string) {
  window.history.pushState({}, "", `${basePath()}${to}`);
  window.dispatchEvent(new PopStateEvent("popstate"));
}

function useRoute(): Route {
  const [route, setRoute] = useState(() => parse(location.pathname, location.search));
  useEffect(() => {
    const onChange = () => setRoute(parse(location.pathname, location.search));
    window.addEventListener("popstate", onChange);
    return () => window.removeEventListener("popstate", onChange);
  }, []);
  return route;
}

/** An in-app link. A real `<a href>`, so middle-click and open-in-new-tab work. */
export function Link({
  to,
  children,
  className = "",
  ...rest
}: { to: string; children: React.ReactNode; className?: string } & React.AnchorHTMLAttributes<HTMLAnchorElement>) {
  return (
    <a
      {...rest}
      href={`${basePath()}${to}`}
      className={className}
      onClick={(event) => {
        // Let the browser handle anything that is not a plain left click.
        if (event.defaultPrevented || event.metaKey || event.ctrlKey || event.shiftKey || event.button !== 0) {
          return;
        }
        event.preventDefault();
        navigate(to);
      }}
    >
      {children}
    </a>
  );
}

// --- live connection ------------------------------------------------------

export type LiveStatus = "connecting" | "live" | "reconnecting" | "offline";

/**
 * One `EventSource` for the whole app, with a coalescing buffer.
 *
 * A burst of transitions — a twenty-job bulk retry — becomes **one** refresh
 * 250ms later rather than twenty renders. `EventSource` reconnects on its own, but
 * it surfaces only an opaque error with no status, so on failure the client asks
 * the session endpoint what actually happened rather than reconnecting forever
 * against a server that now wants a credential.
 */
function useLive(onChange: () => void, onSessionLost: () => void) {
  const [status, setStatus] = useState<LiveStatus>("connecting");
  const pending = useRef<number | null>(null);
  const changeRef = useRef(onChange);
  changeRef.current = onChange;

  useEffect(() => {
    let source: EventSource | null = null;
    let failures = 0;
    let stopped = false;
    let retry: number | undefined;

    const coalesce = () => {
      if (pending.current !== null) return;
      pending.current = window.setTimeout(() => {
        pending.current = null;
        changeRef.current();
      }, 250);
    };

    const open = () => {
      source = new EventSource(api.eventsUrl(), { withCredentials: true });

      source.onopen = () => {
        // A reconnect may have missed transitions, so resync unconditionally.
        if (failures > 0) coalesce();
        failures = 0;
        setStatus("live");
      };

      // Every event is an invalidation hint. `progress` is deliberately its own
      // name so seeding telemetry — which fires per job per poll — can be
      // throttled without throttling real transitions.
      for (const name of ["job", "jobs", "progress", "activity", "trackers", "settings", "gap"]) {
        source.addEventListener(name, coalesce);
      }
      // The auth token changed, so this tab's cookie just died. Without this it
      // would discover that by silently 401ing on its next action.
      source.addEventListener("session", () => onSessionLost());

      source.onerror = () => {
        source?.close();
        if (stopped) return;
        failures += 1;
        if (failures > 5) {
          // Give up on the stream and find out why, rather than reconnecting
          // against a 401 forever.
          setStatus("offline");
          void api
            .session()
            .then((session) => {
              if (!session.authenticated) onSessionLost();
            })
            .catch(() => undefined);
          return;
        }
        setStatus("reconnecting");
        const backoff = Math.min(30_000, 500 * 2 ** (failures - 1));
        retry = window.setTimeout(open, backoff * (0.7 + Math.random() * 0.6));
      };
    };

    open();
    return () => {
      stopped = true;
      source?.close();
      window.clearTimeout(retry);
      if (pending.current !== null) window.clearTimeout(pending.current);
    };
  }, [onSessionLost]);

  return status;
}

// --- chrome ---------------------------------------------------------------

const NAV = [
  { to: "/", label: "Home", glyph: "⌂", match: (r: Route) => r.name === "dashboard" },
  {
    to: "/repairs",
    label: "Repairs",
    glyph: "⇄",
    match: (r: Route) => r.name === "repairs" || r.name === "job",
  },
  { to: "/review", label: "Review", glyph: "▲", match: (r: Route) => r.name === "review" },
  {
    to: "/diagnostics",
    label: "Diagnostics",
    glyph: "◍",
    match: (r: Route) => r.name === "diagnostics",
  },
] as const;

function ThemeToggle() {
  const [theme, setTheme] = useState<"system" | "light" | "dark">(
    () => (localStorage.getItem("seedmedic.theme") as "light" | "dark" | null) ?? "system",
  );

  useEffect(() => {
    if (theme === "system") {
      document.documentElement.removeAttribute("data-theme");
      localStorage.removeItem("seedmedic.theme");
    } else {
      document.documentElement.setAttribute("data-theme", theme);
      localStorage.setItem("seedmedic.theme", theme);
    }
  }, [theme]);

  const next = theme === "system" ? "light" : theme === "light" ? "dark" : "system";
  const glyph = theme === "system" ? "◐" : theme === "light" ? "☀" : "☾";

  return (
    <IconButton
      label={`Theme: ${theme}. Switch to ${next}.`}
      onClick={() => setTheme(next)}
    >
      {glyph}
    </IconButton>
  );
}

function LiveIndicator({ status, generatedAt }: { status: LiveStatus; generatedAt: string | null }) {
  // One shared ticker so forty relative timestamps cost one timer.
  const [, tick] = useState(0);
  useEffect(() => {
    const id = window.setInterval(() => tick((n) => n + 1), 10_000);
    return () => window.clearInterval(id);
  }, []);

  const text =
    status === "live"
      ? "live"
      : status === "connecting"
        ? "connecting…"
        : status === "reconnecting"
          ? "reconnecting…"
          : "offline";
  const tone =
    status === "live" ? "--state-completed" : status === "offline" ? "--state-failed" : "--state-review";

  return (
    <p className="flex items-center gap-1.5 text-[12px]" role="status" aria-live="polite">
      <span aria-hidden="true" style={{ color: `var(${tone})` }}>
        {status === "live" ? "●" : status === "offline" ? "○" : "◐"}
      </span>
      <span style={{ color: "var(--text-muted)" }}>{text}</span>
      {generatedAt && (
        // The seconds counter is hidden from assistive tech: read aloud every ten
        // seconds forever it would be worse than useless.
        <span aria-hidden="true" style={{ color: "var(--text-muted)" }}>
          · updated {relative(generatedAt)}
        </span>
      )}
    </p>
  );
}

export function App() {
  const route = useRoute();
  const [session, setSession] = useState<Session | null>(null);
  const [dashboard, setDashboard] = useState<Dashboard | null>(null);
  const [announcement, setAnnouncement] = useState("");
  const { toasts, push, dismiss } = useToasts();
  const headingRef = useRef<HTMLDivElement>(null);

  const loadSession = useCallback(() => {
    void api
      .session()
      .then(setSession)
      .catch(() => setSession(null));
  }, []);

  const loadDashboard = useCallback(() => {
    void api
      .dashboard()
      .then(setDashboard)
      .catch((error: unknown) => {
        if (error instanceof Unauthorized) navigate("/login");
      });
  }, []);

  useEffect(loadSession, [loadSession]);
  useEffect(loadDashboard, [loadDashboard]);

  const onSessionLost = useCallback(() => {
    setSession(null);
    navigate("/login");
  }, []);

  const authenticated = session?.authenticated ?? true;
  const live = useLive(loadDashboard, onSessionLost);

  // Focus and title follow the route, so a keyboard or screen-reader user is not
  // left at the top of the document after navigating.
  useEffect(() => {
    document.title = `SeedMedic — ${route.name === "job" ? "Repair" : route.name}`;
    headingRef.current?.focus();
  }, [route]);

  const notify = useCallback(
    (tone: "success" | "danger" | "info", message: string, detail?: string) => {
      push(tone, message, detail);
      setAnnouncement(message);
    },
    [push],
  );

  const onError = useCallback(
    (error: unknown) => {
      if (error instanceof Unauthorized) {
        navigate("/login");
        return;
      }
      if (error instanceof ApiError) {
        // The server's own refusal text, verbatim — it says what to do about it.
        notify("danger", error.message);
        return;
      }
      notify("danger", "That did not reach the server. Nothing changed.");
    },
    [notify],
  );

  if (route.name === "login" || (session && !session.authenticated)) {
    return (
      <>
        <LoginScreen
          session={session}
          onSignedIn={() => {
            loadSession();
            loadDashboard();
            navigate("/");
          }}
        />
        <ToastHost toasts={toasts} dismiss={dismiss} />
      </>
    );
  }

  const reviewCount = dashboard?.attention.review ?? 0;
  const warnings = dashboard?.setup.warnings ?? [];

  return (
    <div className="min-h-dvh lg:flex">
      <a href="#main" className="skip-link">
        Skip to main content
      </a>

      {/* Desktop sidebar. */}
      <nav
        aria-label="Sections"
        className="hidden shrink-0 border-r p-3 lg:block lg:w-56"
        style={{ borderColor: "var(--border)", background: "var(--surface)" }}
      >
        <Link
          to="/"
          className="mb-4 flex min-h-11 items-center gap-2 px-2 no-underline"
          style={{ color: "var(--text)" }}
        >
          <span aria-hidden="true" className="text-xl">
            🌱
          </span>
          <span className="font-semibold">SeedMedic</span>
        </Link>
        <ul className="space-y-1">
          {NAV.map((item) => (
            <li key={item.to}>
              <Link
                to={item.to}
                aria-current={item.match(route) ? "page" : undefined}
                className="flex min-h-11 items-center gap-2.5 rounded-[var(--radius-md)] px-2.5 no-underline"
                style={{
                  color: item.match(route) ? "var(--accent-on-soft)" : "var(--text)",
                  background: item.match(route) ? "var(--accent-soft)" : "transparent",
                }}
              >
                <span aria-hidden="true">{item.glyph}</span>
                {item.label}
                {item.to === "/review" && reviewCount > 0 && <Badge tone="review">{reviewCount}</Badge>}
              </Link>
            </li>
          ))}
          <li>
            {/* Settings is still the server-rendered pages; see 0021's status. */}
            <a
              href={`${basePath()}/settings`}
              className="flex min-h-11 items-center gap-2.5 rounded-[var(--radius-md)] px-2.5 no-underline"
              style={{ color: "var(--text)" }}
            >
              <span aria-hidden="true">⚙</span>
              Settings
            </a>
          </li>
        </ul>
      </nav>

      <div className="min-w-0 flex-1">
        <header
          className="sticky top-0 z-20 flex items-center gap-3 border-b px-4 py-2.5 lg:justify-end"
          style={{ borderColor: "var(--border)", background: "var(--surface)" }}
        >
          <Link
            to="/"
            className="flex min-h-11 items-center gap-2 no-underline lg:hidden"
            style={{ color: "var(--text)" }}
          >
            <span aria-hidden="true">🌱</span>
            <span className="font-semibold">SeedMedic</span>
          </Link>
          <div className="ml-auto flex items-center gap-2">
            <LiveIndicator status={live} generatedAt={dashboard?.generated_at ?? null} />
            <ThemeToggle />
            {session?.auth === "set" && (
              <Button
                onClick={() => {
                  void api.signOut().then(() => {
                    setSession(null);
                    navigate("/login");
                  });
                }}
              >
                Sign out
              </Button>
            )}
          </div>
        </header>

        <main id="main" className="mx-auto w-full max-w-4xl px-4 pt-4 pb-24 lg:pb-8">
          <div ref={headingRef} tabIndex={-1} data-route-focus />

          <div className="space-y-3">
            {/* The setup banner: every unmet setting, named, linking to where it
                is fixed. Same data the maud banner used. */}
            {warnings.length > 0 && (
              <Banner
                tone="warning"
                title="Not fully configured"
                action={<LinkButton href={`${basePath()}/settings`} variant="primary">Open settings</LinkButton>}
              >
                <ul className="list-inside list-disc">
                  {warnings.map((warning) => (
                    <li key={warning}>{warning}</li>
                  ))}
                </ul>
              </Banner>
            )}

            {session?.auth === "unset" && (
              <Banner tone="warning" title="No auth token is set">
                Anyone who can reach this port can use the whole UI, settings included.{" "}
                <a href={`${basePath()}/settings/server`}>Set one</a>.
              </Banner>
            )}

            {/* The worker being quiet is a *different* fact from the connection
                being down, and the two must never be conflated. */}
            {dashboard?.worker.stale && (
              <Banner tone="danger" title="The worker has gone quiet">
                Last run {relative(dashboard.worker.last_tick)}. Repairs are not advancing.
              </Banner>
            )}
          </div>

          <div className="mt-4">
            {route.name === "dashboard" && (
              <DashboardScreen dashboard={dashboard} onRefresh={loadDashboard} />
            )}
            {route.name === "repairs" && (
              <RepairsScreen query={route.query} notify={notify} onError={onError} live={live} />
            )}
            {route.name === "review" && (
              <RepairsScreen reviewOnly query={new URLSearchParams()} notify={notify} onError={onError} live={live} />
            )}
            {route.name === "job" && (
              <JobScreen id={route.id} notify={notify} onError={onError} live={live} />
            )}
            {route.name === "diagnostics" && <DiagnosticsScreen onError={onError} />}
            {route.name === "notFound" && (
              <>
                {/* Every route gets exactly one `h1`: a page with none leaves a
                    screen-reader user with nothing to orient by. */}
                <h1 className="mb-3 text-[22px] font-semibold tracking-tight">Page not found</h1>
                <Banner tone="warning">
                  Nothing lives at <code className="break-path">{route.path}</code>.{" "}
                  <Link to="/">Back to the dashboard</Link>.
                </Banner>
              </>
            )}
          </div>
        </main>
      </div>

      {/* Mobile tab bar. Four destinations, which is what fits comfortably. */}
      <nav
        aria-label="Sections"
        className="fixed inset-x-0 bottom-0 z-30 grid grid-cols-5 border-t lg:hidden"
        style={{ borderColor: "var(--border)", background: "var(--surface)" }}
      >
        {NAV.map((item) => (
          <Link
            key={item.to}
            to={item.to}
            aria-current={item.match(route) ? "page" : undefined}
            className="flex min-h-14 flex-col items-center justify-center gap-0.5 text-[11px] no-underline"
            style={{ color: item.match(route) ? "var(--accent)" : "var(--text-muted)" }}
          >
            <span aria-hidden="true" className="relative text-[17px]">
              {item.glyph}
              {item.to === "/review" && reviewCount > 0 && (
                <span
                  aria-hidden="true"
                  className="absolute -top-1 -right-2 rounded-full px-1 text-[10px] font-bold"
                  style={{ background: "var(--state-review)", color: "var(--surface)" }}
                >
                  {reviewCount}
                </span>
              )}
            </span>
            {item.to === "/review" && reviewCount > 0 ? (
              <span>
                {item.label}
                <span className="sr-only">, {reviewCount} needing a decision</span>
              </span>
            ) : (
              item.label
            )}
          </Link>
        ))}
        <a
          href={`${basePath()}/settings`}
          className="flex min-h-14 flex-col items-center justify-center gap-0.5 text-[11px] no-underline"
          style={{ color: "var(--text-muted)" }}
        >
          <span aria-hidden="true" className="text-[17px]">
            ⚙
          </span>
          Settings
        </a>
      </nav>

      <ToastHost toasts={toasts} dismiss={dismiss} />
      <LiveRegion message={announcement} />
    </div>
  );
}
