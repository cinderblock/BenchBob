import { useCallback, useEffect, useRef, useState } from "react";
import { useBob } from "../../bob/context";

/** GET `path` every `ms` while mounted (and the tab is visible); `reload()` fetches now. */
export function usePoll<T>(path: string, ms: number): { data: T | null; error: string; reload(): Promise<void> } {
  const { conn } = useBob();
  const [data, setData] = useState<T | null>(null);
  const [error, setError] = useState("");
  const live = useRef(true);
  const reload = useCallback(async () => {
    try {
      const r = await conn.json<T>("GET", path);
      if (!live.current) return;
      setData(r);
      setError("");
    } catch (e: any) {
      if (live.current) setError(e?.message || "Request failed");
    }
  }, [conn, path]);
  useEffect(() => {
    live.current = true;
    reload();
    const t = setInterval(() => document.visibilityState === "visible" && reload(), ms);
    return () => {
      live.current = false;
      clearInterval(t);
    };
  }, [reload, ms]);
  return { data, error, reload };
}
