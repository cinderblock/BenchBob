// The serial terminal renderer: a small custom one, not xterm.js. A line-oriented view with
// ANSI SGR colours, CR overwrite and backspace, plus a hex view. Incoming bytes are queued
// and rendered once per animation frame, completed lines are appended as a batch, and the
// scrollback is capped, so a device spewing at 2 Mbaud can't lock up the page. It owns the
// screen element's children; React never renders into it.

const MAX_LINES = 4000;
const HIST_MAX = 512 * 1024;
const PALETTE = ["#2e3440", "#ff6b6b", "#5fd38d", "#f5c451", "#6c8cff", "#d67bff", "#4fd1d9", "#d8dee9", "#6b7280", "#ff8f8f", "#86efac", "#fde68a", "#93a8ff", "#e9a8ff", "#8be9f0", "#ffffff"];

const esc = (s: string) => s.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;").replace(/"/g, "&quot;");

function ctl(ch: string) {
  const c = ch.charCodeAt(0);
  return c === 127 ? "␡" : String.fromCharCode(0x2400 + c);
}
function c256(n: number) {
  if (n < 16) return PALETTE[n];
  if (n >= 232) {
    const v = 8 + (n - 232) * 10;
    return `rgb(${v},${v},${v})`;
  }
  n -= 16;
  const k = [0, 95, 135, 175, 215, 255];
  return `rgb(${k[Math.floor(n / 36)]},${k[Math.floor(n / 6) % 6]},${k[n % 6]})`;
}
export function fmtTs(t: number) {
  const d = new Date(t || Date.now());
  return d.toTimeString().slice(0, 8) + "." + String(d.getMilliseconds()).padStart(3, "0");
}

interface Chunk {
  b: Uint8Array;
  t: number;
  tx: boolean;
}
interface Seg {
  x: string;
  s: string;
}

export class Term {
  hex = false;
  ts = false;
  echo = true;
  raf = 0;
  private hist: Chunk[] = [];
  private histBytes = 0;
  private queue: Chunk[] = [];
  private dec!: TextDecoder;
  private escState = 0;
  private csi = "";
  private style = { fg: -1, bg: -1, bold: false, fgc: "", bgc: "" };
  private cur: HTMLDivElement | null = null;
  private segs: Seg[] = [];
  private cr = false;
  private lineT = 0;
  private lastT = 0;
  private hexOff = 0;
  private hexBuf: number[] = [];
  private hexTx = false;
  private hexT = 0;
  private frag: DocumentFragment | null = null;

  constructor(private screen: HTMLElement) {
    this.reset();
  }

  private reset() {
    this.screen.textContent = "";
    this.dec = new TextDecoder("utf-8", { fatal: false });
    this.escState = 0;
    this.csi = "";
    this.style = { fg: -1, bg: -1, bold: false, fgc: "", bgc: "" };
    this.cur = null;
    this.segs = [];
    this.cr = false;
    this.lineT = 0;
    this.hexOff = 0;
    this.hexBuf = [];
    this.hexTx = false;
    this.hexT = 0;
    this.frag = null;
  }

  feed(b: Uint8Array, t: number, tx: boolean) {
    if (tx && !this.echo) return;
    this.hist.push({ b, t, tx });
    this.histBytes += b.length;
    while (this.histBytes > HIST_MAX && this.hist.length > 1) this.histBytes -= this.hist.shift()!.b.length;
    this.queue.push({ b, t, tx });
    if (!this.raf) this.raf = requestAnimationFrame(() => this.frame());
  }
  clear() {
    this.hist = [];
    this.histBytes = 0;
    this.queue = [];
    this.reset();
  }
  rerender() {
    this.queue = this.hist.slice();
    this.reset();
    if (!this.raf) this.raf = requestAnimationFrame(() => this.frame());
  }
  dispose() {
    if (this.raf) cancelAnimationFrame(this.raf);
    this.raf = 0;
  }

  private frame() {
    this.raf = 0;
    const sc = this.screen;
    const stick = sc.scrollTop + sc.clientHeight >= sc.scrollHeight - 8;
    this.frag = document.createDocumentFragment();
    const q = this.queue;
    this.queue = [];
    for (const c of q) this.hex ? this.hexChunk(c) : this.textChunk(c);
    // Completed lines land in one append; the open line is redrawn in place.
    if (this.cur && !this.cur.isConnected) this.frag.appendChild(this.cur);
    sc.appendChild(this.frag);
    this.frag = null;
    if (this.hex) this.drawHexLine();
    else this.drawLine();
    let extra = sc.childElementCount - MAX_LINES;
    while (extra-- > 0) sc.firstChild!.remove();
    if (stick) sc.scrollTop = sc.scrollHeight;
  }
  private newLineEl(t: number) {
    const d = document.createElement("div");
    this.cur = d;
    this.segs = [];
    this.lineT = t;
    return d;
  }
  private endLine() {
    if (!this.cur) this.newLineEl(this.lastT);
    this.drawLine();
    if (!this.cur!.isConnected) this.frag!.appendChild(this.cur!);
    this.cur = null;
  }
  private put(text: string, cls?: string) {
    if (!this.cur) this.newLineEl(this.lastT);
    if (this.cr) {
      this.segs = [];
      this.cr = false;
    }
    const st = cls ? cls : this.styleStr();
    const last = this.segs[this.segs.length - 1];
    if (last && last.s === st) last.x += text;
    else this.segs.push({ x: text, s: st });
  }
  private styleStr() {
    const s = this.style;
    let o = "";
    const fg = s.fgc || (s.fg >= 0 ? PALETTE[s.fg + (s.bold && s.fg < 8 ? 8 : 0)] : "");
    if (fg) o += `color:${fg};`;
    const bg = s.bgc || (s.bg >= 0 ? PALETTE[s.bg] : "");
    if (bg) o += `background:${bg};`;
    if (s.bold) o += "font-weight:700;";
    return o;
  }
  private drawLine() {
    if (!this.cur) return;
    let h = this.ts ? `<span class="ts">${fmtTs(this.lineT)} </span>` : "";
    for (const g of this.segs) h += g.s === "tx" || g.s === "cc" ? `<span class="${g.s}">${esc(g.x)}</span>` : g.s ? `<span style="${g.s}">${esc(g.x)}</span>` : esc(g.x);
    this.cur.innerHTML = h || "​";
  }
  private textChunk({ b, t, tx }: Chunk) {
    this.lastT = t;
    if (tx) {
      // Local echo: shown inline in its own colour, no escape processing.
      const s = new TextDecoder().decode(b);
      for (const ch of s) {
        if (ch === "\n") this.endLine();
        else if (ch === "\r") continue;
        else this.put(ch < " " ? ctl(ch) : ch, "tx");
      }
      return;
    }
    const s = this.dec.decode(b, { stream: true });
    for (const ch of s) {
      const c = ch.charCodeAt(0);
      if (this.escState === 1) {
        // after ESC
        if (ch === "[") {
          this.escState = 2;
          this.csi = "";
        } else if (ch === "]") this.escState = 3;
        else this.escState = 0;
        continue;
      }
      if (this.escState === 2) {
        // CSI params until a final byte
        if (c >= 0x40 && c <= 0x7e) {
          this.escState = 0;
          this.csiDone(ch);
        } else this.csi += ch;
        continue;
      }
      if (this.escState === 3) {
        // OSC until BEL or ST
        if (c === 7 || ch === "\\") this.escState = 0;
        continue;
      }
      if (c === 27) {
        this.escState = 1;
        continue;
      }
      if (ch === "\n") {
        this.cr = false;
        this.endLine();
        continue;
      }
      if (ch === "\r") {
        this.cr = true;
        continue;
      }
      if (c === 8) {
        const g = this.segs[this.segs.length - 1];
        if (g) {
          g.x = g.x.slice(0, -1);
          if (!g.x) this.segs.pop();
        }
        continue;
      }
      if (c === 7) continue;
      if (c < 32 && c !== 9) {
        this.put(ctl(ch), "cc");
        continue;
      }
      this.put(ch);
    }
  }
  private csiDone(f: string) {
    if (f === "m") {
      const p = this.csi === "" ? [0] : this.csi.split(";").map((x) => parseInt(x || "0", 10));
      const s = this.style;
      for (let i = 0; i < p.length; i++) {
        const n = p[i];
        if (n === 0) {
          s.fg = s.bg = -1;
          s.bold = false;
          s.fgc = s.bgc = "";
        } else if (n === 1) s.bold = true;
        else if (n === 22) s.bold = false;
        else if (n >= 30 && n <= 37) {
          s.fg = n - 30;
          s.fgc = "";
        } else if (n >= 90 && n <= 97) {
          s.fg = n - 90 + 8;
          s.fgc = "";
        } else if (n === 39) {
          s.fg = -1;
          s.fgc = "";
        } else if (n >= 40 && n <= 47) {
          s.bg = n - 40;
          s.bgc = "";
        } else if (n >= 100 && n <= 107) {
          s.bg = n - 100 + 8;
          s.bgc = "";
        } else if (n === 49) {
          s.bg = -1;
          s.bgc = "";
        } else if (n === 38 || n === 48) {
          let col = "";
          if (p[i + 1] === 5) {
            col = c256(p[i + 2]);
            i += 2;
          } else if (p[i + 1] === 2) {
            col = `rgb(${p[i + 2] | 0},${p[i + 3] | 0},${p[i + 4] | 0})`;
            i += 4;
          }
          if (n === 38) s.fgc = col;
          else s.bgc = col;
        }
      }
    } else if (f === "J" && (this.csi === "2" || this.csi === "3")) {
      this.screen.textContent = "";
      this.cur = null;
      this.frag = document.createDocumentFragment();
    } else if (f === "K") {
      if (this.cr || this.csi === "2") this.segs = [];
    }
    // Cursor movement and the rest are ignored: this is a log view, not a screen emulator.
  }
  private hexChunk({ b, t, tx }: Chunk) {
    if (tx !== this.hexTx && this.hexBuf.length) this.hexFlush();
    this.hexTx = tx;
    for (const x of b) {
      if (!this.hexBuf.length) this.hexT = t;
      this.hexBuf.push(x);
      if (this.hexBuf.length === 16) this.hexFlush();
    }
  }
  private hexFlush() {
    this.drawHexLine();
    if (this.cur && !this.cur.isConnected) this.frag!.appendChild(this.cur);
    if (!this.hexTx) this.hexOff += this.hexBuf.length;
    this.hexBuf = [];
    this.cur = null;
  }
  private drawHexLine() {
    if (!this.hexBuf.length) return;
    if (!this.cur) this.cur = document.createElement("div");
    const bs = this.hexBuf;
    const hx = bs
      .map((x) => x.toString(16).padStart(2, "0"))
      .join(" ")
      .padEnd(47, " ");
    const asc = bs.map((x) => (x >= 32 && x < 127 ? String.fromCharCode(x) : ".")).join("");
    const lead = this.hexTx ? "  sent  " : this.hexOff.toString(16).padStart(8, "0");
    this.cur.innerHTML = `${this.ts ? `<span class="ts">${fmtTs(this.hexT)} </span>` : ""}<span class="hx-off">${lead}</span>  <span class="${this.hexTx ? "tx" : ""}">${hx}  |${esc(asc)}|</span>`;
  }
}
