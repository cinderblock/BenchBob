// In-place action status: what an action is doing, did, or why it failed, shown beside the
// button or in the card that started it. The dashboard has no global notifications; every
// result lives where its action was taken.

import { useCallback, useMemo, useState, type ReactNode } from "react";

export interface ActionStatus {
  state: "busy" | "ok" | "error";
  msg: ReactNode;
  t: number;
}

/** A thrown request error as text (a TypeError is fetch failing outright). */
export const errText = (e: unknown, fallback = "Request failed"): string => (e instanceof TypeError ? "Request failed: " + e.message : (e as Error)?.message || fallback);

export interface Action {
  status: ActionStatus | null;
  busy: boolean;
  /** Show progress; the caller's buttons should be disabled while `busy`. */
  progress(msg: ReactNode): void;
  ok(msg: ReactNode): void;
  /** An error (thrown value or text). */
  fail(e: unknown): void;
  set(s: ActionStatus | null): void;
  clear(): void;
  /**
   * Run `fn`, showing `busy` while it runs, then `ok` (text or from the result) or the error.
   * Resolves the result, or undefined when it failed. Never throws.
   */
  run<T>(fn: () => Promise<T>, o?: { busy?: ReactNode; ok?: ReactNode | ((r: T) => ReactNode) }): Promise<T | undefined>;
}

export function useAction(): Action {
  const [status, set] = useState<ActionStatus | null>(null);
  const put = useCallback((state: ActionStatus["state"], msg: ReactNode) => set({ state, msg, t: Date.now() }), []);
  const a = useMemo(
    () => ({
      progress: (msg: ReactNode) => put("busy", msg),
      ok: (msg: ReactNode) => put("ok", msg),
      fail: (e: unknown) => put("error", typeof e === "string" ? e : errText(e)),
      set,
      clear: () => set(null),
      async run<T>(fn: () => Promise<T>, o: { busy?: ReactNode; ok?: ReactNode | ((r: T) => ReactNode) } = {}) {
        put("busy", o.busy ?? "Working…");
        try {
          const r = await fn();
          const msg = typeof o.ok === "function" ? (o.ok as (r: T) => ReactNode)(r) : o.ok;
          if (msg != null && msg !== false) put("ok", msg);
          else set(null);
          return r;
        } catch (e) {
          put("error", errText(e));
          return undefined;
        }
      },
    }),
    [put],
  );
  return { ...a, status, busy: status?.state === "busy" };
}

/**
 * One action's status as an inline line: busy text, "✓ done at 14:02:11", or the error in red.
 * Done and error lines carry a dismiss button.
 */
export function StatusLine({ s, onClear, time = true }: { s: ActionStatus | null; onClear?(): void; time?: boolean }) {
  if (!s) return null;
  if (s.state === "busy")
    return (
      <span className="act-stat busy" role="status">
        {s.msg}
      </span>
    );
  const at = new Date(s.t).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit", second: "2-digit" });
  return (
    <span className={`act-stat ${s.state}`} role={s.state === "error" ? "alert" : "status"}>
      {s.state === "ok" ? "✓ " : "✗ "}
      {s.msg}
      {s.state === "ok" && time && <span className="act-at"> at {at}</span>}
      {onClear && (
        <button type="button" className="pr-x" aria-label="Dismiss" onClick={onClear}>
          &times;
        </button>
      )}
    </span>
  );
}

/** `useAction()`'s status line, wired to its own dismiss. */
export const ActionLine = ({ a, time }: { a: Action; time?: boolean }) => <StatusLine s={a.status} onClear={a.clear} time={time} />;
