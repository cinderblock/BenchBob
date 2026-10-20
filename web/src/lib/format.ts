// Number and time formatting shared by every section, as the old dashboard had them.

export function fmtSz(b: number | null | undefined): string {
  const n = Number(b);
  if (!n) return "0 B";
  const u = ["B", "KB", "MB", "GB", "TB"];
  const i = Math.min(Math.floor(Math.log(n) / Math.log(1024)), u.length - 1);
  return (n / Math.pow(1024, i)).toFixed(i > 0 ? 1 : 0) + " " + u[i];
}

export function fmtRate(b: number | null | undefined): string {
  const n = Number(b);
  if (!n || n < 1) return "0 B/s";
  const u = ["B/s", "KB/s", "MB/s", "GB/s"];
  const i = Math.min(Math.floor(Math.log(n) / Math.log(1024)), u.length - 1);
  return (n / Math.pow(1024, i)).toFixed(i > 0 ? 1 : 0) + " " + u[i];
}

export const fmtRam = (mb: number) => (mb >= 1024 ? `${mb / 1024} GB` : `${mb} MB`);

export function fmtUp(sec: number): string {
  const s = Math.floor(sec);
  const d = Math.floor(s / 86400),
    h = Math.floor((s % 86400) / 3600),
    m = Math.floor((s % 3600) / 60);
  return "up " + (d ? d + "d " : "") + (d || h ? h + "h " : "") + m + "m";
}

export function fmtAgo(ms: number, now = Date.now()): string {
  const s = Math.max(0, Math.round((now - ms) / 1000));
  return s < 60 ? s + "s ago" : s < 3600 ? Math.floor(s / 60) + "m ago" : s < 86400 ? Math.floor(s / 3600) + "h ago" : Math.floor(s / 86400) + "d ago";
}

export const fmtHex = (n: number, w = 2) => "0x" + Number(n).toString(16).toUpperCase().padStart(w, "0");

/** Bytes as B / KB / MB, one decimal: the panels' compact size. */
export const fmtN = (n: number) => (n < 1024 ? n + " B" : n < 1048576 ? (n / 1024).toFixed(1) + " KB" : (n / 1048576).toFixed(1) + " MB");

export function pctColor(pct: number): string {
  if (pct < 60) return "#6c8cff";
  if (pct < 85) return "#f5a623";
  return "#ff5c5c";
}

export function wifiColor(dbm: number): string {
  if (dbm >= -50) return "#4caf7d";
  if (dbm >= -67) return "#6c8cff";
  if (dbm >= -75) return "#f5a623";
  return "#ff5c5c";
}

export const cap = (s: string) => (s ? s[0].toUpperCase() + s.slice(1) : "");

/** hexdump -C style: offset, 16 bytes, ASCII. */
export function hexdump(bytes: (number | null)[], start = 0): string {
  let out = "";
  for (let i = 0; i < bytes.length; i += 16) {
    const row = bytes.slice(i, i + 16);
    out +=
      (start + i).toString(16).padStart(4, "0") +
      "  " +
      row.map((b) => (b == null ? "--" : b.toString(16).padStart(2, "0"))).join(" ").padEnd(48) +
      "  " +
      row.map((b) => (b == null ? " " : b >= 32 && b < 127 ? String.fromCharCode(b) : ".")).join("") +
      "\n";
  }
  return out;
}

export const plural = (n: number, one: string, many = one + "s") => `${n} ${n === 1 ? one : many}`;
