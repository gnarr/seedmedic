/**
 * Sign in.
 *
 * A client route rather than a server-rendered page, which is what forces the
 * shell and its assets to be served unauthenticated: guarding them would give
 * `/` → `/login` → shell → `/assets/app.js` → 401 → a blank page with no way in.
 * The bundle carries no operator data, so there is nothing to protect there.
 */

import { useState } from "react";
import { ApiError, api, type Session } from "../api";
import { Banner, Button, Card } from "../ui";

export function LoginScreen({
  session,
  onSignedIn,
}: {
  session: Session | null;
  onSignedIn: () => void;
}) {
  const [token, setToken] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const submit = async (event: React.FormEvent) => {
    event.preventDefault();
    setBusy(true);
    setError(null);
    try {
      await api.signIn(token);
      // Never keep the token in component state after it has been exchanged for
      // a cookie, and never in web storage — the cookie is HttpOnly deliberately.
      setToken("");
      onSignedIn();
    } catch (caught) {
      setError(caught instanceof ApiError ? caught.message : "Could not reach the server.");
    } finally {
      setBusy(false);
    }
  };

  return (
    <main id="main" className="mx-auto flex min-h-dvh w-full max-w-sm flex-col justify-center px-4">
      <div className="mb-5 flex items-center gap-2">
        <span aria-hidden="true" className="text-2xl">🌱</span>
        <h1 className="text-[21px] font-semibold">SeedMedic</h1>
      </div>

      <Card>
        {session?.auth === "unset" ? (
          <Banner tone="info" title="Nothing to sign in to">
            No auth token is configured, so the UI is open to anyone who can reach this port.
          </Banner>
        ) : (
          <form onSubmit={(event) => void submit(event)} className="space-y-3">
            <div>
              <label htmlFor="token" className="mb-1 block text-[14px] font-medium">
                Auth token
              </label>
              <input
                id="token"
                type="password"
                autoComplete="current-password"
                autoFocus
                value={token}
                onChange={(event) => setToken(event.target.value)}
                aria-describedby={error ? "token-error" : "token-help"}
                aria-invalid={error !== null}
                className="min-h-[var(--control-h)] w-full rounded-[var(--radius-md)] border px-3 text-[14px]"
                style={{
                  background: "var(--surface)",
                  borderColor: error ? "var(--state-failed)" : "var(--border-strong)",
                  color: "var(--text)",
                }}
              />
              {error ? (
                <p id="token-error" role="alert" className="mt-1.5 text-[13px]" style={{ color: "var(--state-failed)" }}>
                  {error}
                </p>
              ) : (
                <p id="token-help" className="mt-1.5 text-[13px]" style={{ color: "var(--text-muted)" }}>
                  The shared secret from <code>server.auth_token</code>. There are no accounts.
                </p>
              )}
            </div>
            <Button type="submit" variant="primary" block disabled={busy || token.length === 0}>
              {busy ? "Signing in…" : "Sign in"}
            </Button>
          </form>
        )}
      </Card>
    </main>
  );
}
