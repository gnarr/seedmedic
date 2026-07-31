/**
 * The component inventory.
 *
 * Hand-written rather than a component library, and native `<dialog>` rather than
 * a JS modal: `<dialog>.showModal()` gives focus trapping, Escape-to-close,
 * inert-ing the rest of the page and a `::backdrop` from the platform, which is
 * the whole reason a dialog primitive is usually a dependency. Verified with axe
 * in the Playwright suite rather than assumed.
 */

import {
  useEffect,
  useId,
  useRef,
  useState,
  type ButtonHTMLAttributes,
  type ReactNode,
} from "react";
import type { RepairState } from "./api";
import { stateGlyph, stateLabel, stateTone } from "./format";

// --- chips and badges -----------------------------------------------------

export function StateChip({ state, size = "md" }: { state: RepairState; size?: "sm" | "md" }) {
  const tone = stateTone(state);
  return (
    <span
      className={`inline-flex shrink-0 items-center gap-1.5 rounded-full border font-medium ${
        size === "sm" ? "px-2 py-0.5 text-[12px]" : "px-2.5 py-1 text-[13px]"
      }`}
      style={{
        color: `var(--state-${tone})`,
        background: `var(--state-${tone}-bg)`,
        borderColor: `var(--state-${tone})`,
      }}
    >
      <span aria-hidden="true">{stateGlyph(state)}</span>
      {stateLabel(state)}
    </span>
  );
}

export function Badge({
  children,
  tone = "neutral",
}: {
  children: ReactNode;
  tone?: "neutral" | "accent" | "review" | "failed";
}) {
  const color = tone === "accent" ? "var(--accent-on-soft)" : `var(--state-${tone})`;
  const background = tone === "accent" ? "var(--accent-soft)" : `var(--state-${tone}-bg)`;
  return (
    <span
      className="inline-flex items-center gap-1 rounded-full px-2 py-0.5 text-[12px] font-semibold"
      style={{ color, background }}
    >
      {children}
    </span>
  );
}

// --- surfaces -------------------------------------------------------------

export function Card({
  children,
  className = "",
  as: As = "div",
}: {
  children: ReactNode;
  className?: string;
  as?: "div" | "section" | "li" | "article";
}) {
  return (
    <As
      className={`rounded-[var(--radius-lg)] border p-4 ${className}`}
      style={{
        background: "var(--surface)",
        borderColor: "var(--border)",
        boxShadow: "var(--shadow-card)",
      }}
    >
      {children}
    </As>
  );
}

export function SectionHeading({ children, action }: { children: ReactNode; action?: ReactNode }) {
  return (
    <div className="mb-3 flex items-baseline justify-between gap-3">
      <h2 className="text-[17px] font-semibold tracking-tight">{children}</h2>
      {action}
    </div>
  );
}

// --- controls -------------------------------------------------------------

type ButtonProps = ButtonHTMLAttributes<HTMLButtonElement> & {
  variant?: "primary" | "secondary" | "danger" | "ghost";
  block?: boolean;
  /** React 19 passes `ref` as an ordinary prop to function components. */
  ref?: React.Ref<HTMLButtonElement>;
};

export function Button({
  variant = "secondary",
  block = false,
  className = "",
  ...rest
}: ButtonProps) {
  const styles: Record<string, React.CSSProperties> = {
    primary: {
      background: "var(--accent)",
      color: "var(--accent-contrast)",
      borderColor: "var(--accent)",
    },
    secondary: {
      background: "var(--surface)",
      color: "var(--text)",
      borderColor: "var(--border-strong)",
    },
    danger: {
      background: "var(--state-failed-bg)",
      color: "var(--state-failed)",
      borderColor: "var(--state-failed)",
    },
    ghost: { background: "transparent", color: "var(--text-muted)", borderColor: "transparent" },
  };

  return (
    <button
      {...rest}
      // min-h rather than a fixed height, so a wrapped label never clips — and
      // 44px below the tablet breakpoint via --control-h, which is the WCAG 2.2
      // target-size minimum the old 34px buttons failed.
      className={`inline-flex min-h-[var(--control-h)] items-center justify-center gap-2 rounded-[var(--radius-md)] border px-4 text-[14px] font-medium transition-[background,border-color,opacity] duration-[120ms] hover:opacity-90 disabled:cursor-not-allowed disabled:opacity-45 ${
        block ? "w-full" : ""
      } ${className}`}
      style={{ ...styles[variant], minWidth: 44 }}
    />
  );
}

/**
 * A link that looks like a button.
 *
 * Exists because `<a><button/></a>` is invalid HTML — interactive content inside a
 * link — and because the anchor then measures as an inline box, so the 44px target
 * the button has is not the target the browser actually offers.
 */
export function LinkButton({
  href,
  onClick,
  variant = "secondary",
  block = false,
  children,
}: {
  href: string;
  onClick?: (event: React.MouseEvent<HTMLAnchorElement>) => void;
  variant?: "primary" | "secondary";
  block?: boolean;
  children: ReactNode;
}) {
  const primary = variant === "primary";
  return (
    <a
      href={href}
      onClick={onClick}
      className={`inline-flex min-h-[var(--control-h)] items-center justify-center gap-2 rounded-[var(--radius-md)] border px-4 text-[14px] font-medium no-underline ${
        block ? "w-full" : ""
      }`}
      style={{
        background: primary ? "var(--accent)" : "var(--surface)",
        color: primary ? "var(--accent-contrast)" : "var(--text)",
        borderColor: primary ? "var(--accent)" : "var(--border-strong)",
      }}
    >
      {children}
    </a>
  );
}

/** An icon-only control. `label` is required, not optional — that is the point. */
export function IconButton({
  label,
  children,
  ...rest
}: ButtonHTMLAttributes<HTMLButtonElement> & { label: string; children: ReactNode }) {
  return (
    <button
      {...rest}
      aria-label={label}
      title={label}
      className="inline-flex items-center justify-center rounded-[var(--radius-md)] border text-[15px]"
      style={{
        minWidth: 44,
        minHeight: 44,
        background: "transparent",
        borderColor: "var(--border)",
        color: "var(--text-muted)",
      }}
    >
      {children}
    </button>
  );
}

export function Banner({
  tone = "warning",
  title,
  children,
  action,
}: {
  tone?: "warning" | "danger" | "success" | "info";
  title?: string;
  children: ReactNode;
  action?: ReactNode;
}) {
  const map = {
    warning: ["--state-review", "--state-review-bg", "⚠"],
    danger: ["--state-failed", "--state-failed-bg", "✕"],
    // A success variant is new: the old UI had only amber and red, so a
    // connection test that *passed* rendered in the same box as one that failed.
    success: ["--state-completed", "--state-completed-bg", "✓"],
    info: ["--state-seeding", "--state-seeding-bg", "ℹ"],
  } as const;
  const [color, background, glyph] = map[tone];

  return (
    <div
      className="flex items-start gap-3 rounded-[var(--radius-md)] border p-3 text-[14px]"
      style={{ color: `var(${color})`, background: `var(${background})`, borderColor: `var(${color})` }}
      role={tone === "danger" ? "alert" : undefined}
      data-notice
    >
      <span aria-hidden="true" className="mt-px shrink-0">
        {glyph}
      </span>
      <div className="min-w-0 flex-1">
        {title && <p className="font-semibold">{title}</p>}
        {/* A `<p>`, not a `<div>`: it makes the prose a block of text, which is
            what exempts an inline link inside it from the 44px target minimum —
            and what makes that exemption legible to a checker. */}
        <p className="break-path">{children}</p>
      </div>
      {action && <div className="shrink-0">{action}</div>}
    </div>
  );
}

export function EmptyState({
  glyph,
  title,
  children,
  action,
}: {
  glyph: string;
  title: string;
  children?: ReactNode;
  action?: ReactNode;
}) {
  return (
    <div className="flex flex-col items-center gap-2 px-4 py-12 text-center">
      <div aria-hidden="true" className="text-4xl">
        {glyph}
      </div>
      <p className="text-[16px] font-semibold">{title}</p>
      {children && (
        <p className="max-w-prose text-[14px]" style={{ color: "var(--text-muted)" }}>
          {children}
        </p>
      )}
      {action && <div className="mt-2">{action}</div>}
    </div>
  );
}

export function Progress({
  value,
  max = 1,
  label,
  tone = "accent",
}: {
  value: number | null;
  max?: number;
  label: string;
  tone?: string;
}) {
  const indeterminate = value === null;
  const pct = indeterminate ? 0 : Math.max(0, Math.min(100, (value / max) * 100));
  return (
    <div
      role="progressbar"
      aria-label={label}
      aria-valuemin={0}
      aria-valuemax={100}
      {...(indeterminate ? {} : { "aria-valuenow": Math.round(pct) })}
      aria-valuetext={indeterminate ? "in progress" : `${Math.round(pct)} percent`}
      className="h-1.5 w-full overflow-hidden rounded-full"
      style={{ background: "var(--surface-3)" }}
    >
      <div
        className={indeterminate ? "pulse h-full w-1/3" : "h-full transition-[width] duration-500"}
        style={{
          width: indeterminate ? undefined : `${pct}%`,
          background: tone === "accent" ? "var(--accent)" : `var(--state-${tone})`,
        }}
      />
    </div>
  );
}

/**
 * The lifecycle as a stepper.
 *
 * `total` comes from the server (`PROGRESSION.len()`), so this cannot drift from
 * the state machine. A screen reader gets one sentence rather than nine bullets.
 */
export function Stepper({ rank, total, state }: { rank: number | null; total: number; state: RepairState }) {
  if (rank === null) {
    return (
      <p className="text-[13px]" style={{ color: "var(--text-muted)" }}>
        Off the normal path — {stateLabel(state).toLowerCase()}.
      </p>
    );
  }
  return (
    <div>
      <ol className="flex items-center gap-1" aria-hidden="true">
        {Array.from({ length: total }, (_, index) => (
          <li
            key={index}
            className="h-1.5 flex-1 rounded-full"
            style={{
              background: index <= rank ? `var(--state-${stateTone(state)})` : "var(--surface-3)",
            }}
          />
        ))}
      </ol>
      <p className="mt-1.5 text-[13px] nums" style={{ color: "var(--text-muted)" }}>
        Step {rank + 1} of {total} · {stateLabel(state).toLowerCase()}
      </p>
    </div>
  );
}

export function Skeleton({ rows = 3 }: { rows?: number }) {
  return (
    <div className="space-y-2" aria-hidden="true">
      {Array.from({ length: rows }, (_, index) => (
        <div
          key={index}
          className="pulse h-16 rounded-[var(--radius-lg)]"
          style={{ background: "var(--surface-2)" }}
        />
      ))}
    </div>
  );
}

// --- dialog ---------------------------------------------------------------

/**
 * A modal built on `<dialog>`.
 *
 * Focus starts on **Cancel** for a destructive action, so the dangerous button is
 * never one stray Enter away — and the platform returns focus to the trigger when
 * it closes.
 */
export function Dialog({
  open,
  onClose,
  title,
  children,
  footer,
  destructive = false,
}: {
  open: boolean;
  onClose: () => void;
  title: string;
  children: ReactNode;
  footer?: ReactNode;
  destructive?: boolean;
}) {
  const ref = useRef<HTMLDialogElement>(null);
  const cancelRef = useRef<HTMLButtonElement>(null);
  const titleId = useId();

  useEffect(() => {
    const node = ref.current;
    if (!node) return;
    if (open && !node.open) {
      node.showModal();
      if (destructive) cancelRef.current?.focus();
    } else if (!open && node.open) {
      node.close();
    }
  }, [open, destructive]);

  return (
    <dialog
      ref={ref}
      aria-labelledby={titleId}
      // Escape fires `cancel`; keep React's state in step with the platform's.
      onCancel={(event) => {
        event.preventDefault();
        onClose();
      }}
      onClose={onClose}
      className="rise m-auto w-[calc(100vw-2rem)] max-w-lg rounded-[var(--radius-lg)] border p-0"
      style={{
        background: "var(--surface)",
        borderColor: "var(--border-strong)",
        color: "var(--text)",
        boxShadow: "var(--shadow-overlay)",
      }}
    >
      <div className="flex items-start justify-between gap-3 border-b p-4" style={{ borderColor: "var(--border)" }}>
        <h2 id={titleId} className="text-[16px] font-semibold">
          {title}
        </h2>
        <IconButton label="Close" onClick={onClose}>
          ✕
        </IconButton>
      </div>
      <div className="space-y-3 p-4 text-[14px]">{children}</div>
      <div
        className="flex flex-wrap justify-end gap-2 border-t p-4"
        style={{ borderColor: "var(--border)" }}
      >
        <Button ref={cancelRef} onClick={onClose}>
          Cancel
        </Button>
        {footer}
      </div>
    </dialog>
  );
}

// --- toasts ---------------------------------------------------------------

export interface Toast {
  id: number;
  tone: "success" | "danger" | "info";
  message: string;
  detail?: string;
}

let nextToastId = 1;

export function useToasts() {
  const [toasts, setToasts] = useState<Toast[]>([]);

  const push = (tone: Toast["tone"], message: string, detail?: string) => {
    const id = nextToastId++;
    // Capped at three and newest-last: twenty jobs moving at once must not become
    // twenty toasts.
    setToasts((current) => [...current.slice(-2), { id, tone, message, detail }]);
    if (tone !== "danger") {
      window.setTimeout(() => setToasts((c) => c.filter((t) => t.id !== id)), 5000);
    }
  };

  const dismiss = (id: number) => setToasts((c) => c.filter((t) => t.id !== id));

  return { toasts, push, dismiss };
}

export function ToastHost({ toasts, dismiss }: { toasts: Toast[]; dismiss: (id: number) => void }) {
  return (
    <div
      // Sits above the mobile tab bar so it never covers the nav.
      className="pointer-events-none fixed inset-x-0 bottom-20 z-50 flex flex-col items-center gap-2 px-4 md:bottom-4 md:items-end"
      aria-live="polite"
      aria-atomic="false"
    >
      {toasts.map((toast) => (
        <div
          key={toast.id}
          role={toast.tone === "danger" ? "alert" : "status"}
          className="rise pointer-events-auto w-full max-w-sm rounded-[var(--radius-md)] border p-3 text-[14px]"
          style={{
            background: "var(--surface)",
            borderColor: `var(--state-${toast.tone === "success" ? "completed" : toast.tone === "danger" ? "failed" : "seeding"})`,
            boxShadow: "var(--shadow-overlay)",
          }}
        >
          <div className="flex items-start gap-2">
            <span aria-hidden="true">
              {toast.tone === "success" ? "✓" : toast.tone === "danger" ? "✕" : "ℹ"}
            </span>
            <div className="min-w-0 flex-1">
              <p className="font-medium break-path">{toast.message}</p>
              {toast.detail && (
                <p className="mt-0.5 break-path" style={{ color: "var(--text-muted)" }}>
                  {toast.detail}
                </p>
              )}
            </div>
            <IconButton label="Dismiss" onClick={() => dismiss(toast.id)}>
              ✕
            </IconButton>
          </div>
        </div>
      ))}
    </div>
  );
}

/** A single shared polite live region, so announcements do not fight each other. */
export function LiveRegion({ message }: { message: string }) {
  return (
    <p className="sr-only" role="status" aria-live="polite">
      {message}
    </p>
  );
}
