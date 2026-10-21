// Small building blocks used across sections.

import { useState, type CSSProperties, type ReactNode } from "react";
import { ActionLine, useAction } from "./status";

export function Pill({ cls = "", children }: { cls?: string; children: ReactNode }) {
  return <span className={`pr-pill ${cls}`}>{children}</span>;
}

/**
 * Tap-to-expand help, instead of a hover tooltip (invisible on touch screens): a small ⓘ
 * button next to whatever it explains, and the explanation as a line of text below.
 */
export function Hint({ children, label = "What's this?" }: { children: ReactNode; label?: string }) {
  const [open, setOpen] = useState(false);
  return (
    <>
      <button type="button" className={`hint-btn${open ? " on" : ""}`} aria-label={label} aria-expanded={open} onClick={() => setOpen(!open)}>
        i
      </button>
      {open && <div className="hint" style={{ flexBasis: "100%" }}>{children}</div>}
    </>
  );
}

/** Same, but the trigger and the text are given separately (text placed elsewhere). */
export function useToggle(initial = false): [boolean, () => void] {
  const [v, setV] = useState(initial);
  return [v, () => setV((x) => !x)];
}

/** Red/amber/plain notes. */
export const Note = ({ children, color, style }: { children: ReactNode; color?: "danger" | "warning" | "success"; style?: CSSProperties }) => (
  <div className="pr-note" style={{ ...(color ? { color: `var(--${color})` } : {}), ...style }}>
    {children}
  </div>
);

/**
 * A button that disables itself while its async action runs, then shows beside itself why it
 * failed, or `ok` (text, or made from the action's result) when it worked.
 */
export function AsyncButton<T>({ onClick, className = "btn btn-sm", children, disabled, busyText, ok }: { onClick: () => Promise<T> | void; className?: string; children: ReactNode; disabled?: boolean; busyText?: ReactNode; ok?: ReactNode | ((r: T) => ReactNode) }) {
  const a = useAction();
  return (
    <>
      <button className={className} disabled={disabled || a.busy} onClick={() => a.run(async () => (await onClick()) as T, { busy: "", ok })}>
        {a.busy && busyText ? busyText : children}
      </button>
      {a.status?.state !== "busy" && <ActionLine a={a} />}
    </>
  );
}

/** Coloured +/− diff lines (config.txt stage previews). */
export function Diff({ text }: { text: string }) {
  return (
    <div className="pr-log gp-diff">
      {text.split("\n").map((l, i) => (
        <span key={i}>
          <span className={l.startsWith("+") && !l.startsWith("+++") ? "a" : l.startsWith("-") && !l.startsWith("---") ? "d" : l.startsWith("@@") ? "h" : ""}>{l}</span>
          {"\n"}
        </span>
      ))}
    </div>
  );
}
