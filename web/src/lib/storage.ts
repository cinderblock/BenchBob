// localStorage, scoped per bob. A bob serving this page keeps the old dashboard's keys (so
// saved choices carry over from it); any other bob gets the key suffixed with its base URL,
// so two bobs in one page don't share, say, which panels are open.

import { useCallback, useState } from "react";
import type { BobConnection } from "./connection";

export function scopedKey(conn: BobConnection | null, key: string): string {
  return !conn || conn.sameOrigin ? key : `${key}@${conn.baseUrl}`;
}

export function readLocal<T>(key: string, fallback: T): T {
  try {
    const v = localStorage.getItem(key);
    return v == null ? fallback : (JSON.parse(v) as T);
  } catch {
    return fallback;
  }
}

/** Plain string values (the old dashboard stored some unquoted). */
export function readLocalStr(key: string): string | null {
  try {
    return localStorage.getItem(key);
  } catch {
    return null;
  }
}

export function writeLocal(key: string, v: unknown) {
  try {
    if (v === undefined || v === null) localStorage.removeItem(key);
    else localStorage.setItem(key, typeof v === "string" ? v : JSON.stringify(v));
  } catch {}
}

/** useState backed by localStorage (JSON). */
export function useLocal<T>(key: string, initial: T): [T, (v: T | ((p: T) => T)) => void] {
  const [v, setV] = useState<T>(() => readLocal(key, initial));
  const set = useCallback(
    (n: T | ((p: T) => T)) =>
      setV((p) => {
        const next = typeof n === "function" ? (n as (p: T) => T)(p) : n;
        writeLocal(key, JSON.stringify(next));
        return next;
      }),
    [key],
  );
  return [v, set];
}
