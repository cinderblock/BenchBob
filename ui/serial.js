// ==================== Serial ports (dashboard) ====================
// One card per port. The card that has its terminal open holds a WebSocket to
// /ws/serial/:id (binary = raw bytes both ways, JSON text = control/status); every other
// part of the section is driven by the 2 s status push. Controls go through the REST API so
// failures toast the same way as the rest of the dashboard.
//
// The terminal is a small custom renderer, not xterm.js: a line-oriented view with ANSI SGR
// colours, CR overwrite and backspace, plus a hex view. Incoming bytes are queued and
// rendered once per animation frame, completed lines are appended as a batch, and the
// scrollback is capped, so a device spewing at 2 Mbaud can't lock up the page.
(() => {
  const MAX_LINES = 4000;
  const HIST_MAX = 512 * 1024;
  const BAUDS = [300, 1200, 2400, 4800, 9600, 14400, 19200, 38400, 57600, 74880, 115200, 230400, 250000, 460800, 500000, 921600, 1000000, 1500000, 2000000, 3000000];
  const RESET_LABELS = { esp: 'Reset (ESP)', 'esp-boot': 'Reset into bootloader', dtr: 'Reset (DTR pulse)', touch1200: '1200 bps touch (bootloader)' };
  const PALETTE = ['#2e3440', '#ff6b6b', '#5fd38d', '#f5c451', '#6c8cff', '#d67bff', '#4fd1d9', '#d8dee9', '#6b7280', '#ff8f8f', '#86efac', '#fde68a', '#93a8ff', '#e9a8ff', '#8be9f0', '#ffffff'];

  if (!document.getElementById('ser-css')) {
    const css = document.createElement('style');
    css.id = 'ser-css';
    css.textContent = `
.ser-card.gone{opacity:.55}
.ser-card h4 .pr-pill{margin-left:2px}
.ser-meta{font-size:.75rem;color:var(--dim);display:flex;flex-wrap:wrap;gap:4px 12px}
.ser-screen{background:#07080b;border:1px solid var(--border);border-radius:4px;font-family:'SF Mono',ui-monospace,monospace;font-size:.75rem;line-height:1.35;padding:6px 8px;height:min(50vh,420px);overflow:auto;white-space:pre-wrap;word-break:break-all;outline:none;margin-top:8px}
.ser-screen.raw{border-color:var(--accent);box-shadow:0 0 0 1px var(--accent)}
.ser-screen .ts{color:var(--dim)}
.ser-screen .tx{color:#93a8ff}
.ser-screen .cc{color:var(--dim)}
.ser-screen .hx-off{color:var(--dim)}
.ser-in{flex-wrap:nowrap!important}
.ser-in input.ser-line{flex:1;min-width:0;font-family:'SF Mono',ui-monospace,monospace}
.ser-lamp{display:inline-flex;align-items:center;gap:3px;font-size:.7rem;color:var(--dim)}
.ser-lamp i{width:8px;height:8px;border-radius:50%;background:var(--surface3);display:inline-block}
.ser-lamp.on i{background:var(--success)}
.ser-tog.on{background:var(--accent);border-color:var(--accent);color:#fff}
.ser-more{margin-top:8px}
.ser-more summary{cursor:pointer;font-size:.8rem;color:var(--dim);padding:4px 0}
.ser-cmds{display:flex;flex-direction:column;gap:3px;margin:4px 0 8px}
.ser-cmds .pr-endpoint{display:block;overflow-x:auto;white-space:nowrap}
.ser-files .pr-row{margin:4px 0}
.ser-files input[type=file]{max-width:100%}
.ser-sep{width:1px;align-self:stretch;background:var(--border)}
@media(max-width:600px){.ser-sep{display:none}.ser-screen{font-size:.7rem}}`;
    document.head.appendChild(css);
  }

  let root = null, ST = null;
  let selected = localStorage.getItem('serialTerm') || '';
  let term = null; // { id, ws, view, ... } for the selected port

  // ---------- Terminal renderer ----------

  class Term {
    constructor(screen) {
      this.screen = screen;
      this.hex = false; this.ts = false; this.echo = true;
      this.hist = []; this.histBytes = 0;
      this.queue = []; this.raf = 0;
      this.reset();
    }
    reset() {
      this.screen.textContent = '';
      this.dec = new TextDecoder('utf-8', { fatal: false });
      this.esc = 0; this.csi = ''; this.style = { fg: -1, bg: -1, bold: false, fgc: '', bgc: '' };
      this.cur = null; this.segs = []; this.cr = false; this.lineT = 0; this.lineTx = false;
      this.hexOff = 0; this.hexBuf = []; this.hexTx = false; this.hexT = 0;
      this.frag = null;
    }
    feed(b, t, tx) {
      if (tx && !this.echo) return;
      this.hist.push({ b, t, tx }); this.histBytes += b.length;
      while (this.histBytes > HIST_MAX && this.hist.length > 1) this.histBytes -= this.hist.shift().b.length;
      this.queue.push({ b, t, tx });
      if (!this.raf) this.raf = requestAnimationFrame(() => this.frame());
    }
    clear() { this.hist = []; this.histBytes = 0; this.queue = []; this.reset(); }
    rerender() { this.queue = this.hist.slice(); this.reset(); if (!this.raf) this.raf = requestAnimationFrame(() => this.frame()); }
    frame() {
      this.raf = 0;
      const sc = this.screen;
      const stick = sc.scrollTop + sc.clientHeight >= sc.scrollHeight - 8;
      this.frag = document.createDocumentFragment();
      const q = this.queue; this.queue = [];
      for (const c of q) this.hex ? this.hexChunk(c) : this.textChunk(c);
      // Completed lines land in one append; the open line is redrawn in place.
      if (this.cur && !this.cur.isConnected) this.frag.appendChild(this.cur);
      sc.appendChild(this.frag);
      this.frag = null;
      if (this.hex) this.drawHexLine(); else this.drawLine();
      let extra = sc.childElementCount - MAX_LINES;
      while (extra-- > 0) sc.firstChild.remove();
      if (stick) sc.scrollTop = sc.scrollHeight;
    }
    newLineEl(t, tx) {
      const d = document.createElement('div');
      this.cur = d; this.segs = []; this.lineT = t; this.lineTx = tx;
      return d;
    }
    endLine() {
      if (!this.cur) this.newLineEl(this.lastT, false);
      this.drawLine();
      if (!this.cur.isConnected) this.frag.appendChild(this.cur);
      this.cur = null;
    }
    put(text, cls) {
      if (!this.cur) this.newLineEl(this.lastT, this.lastTx);
      if (this.cr) { this.segs = []; this.cr = false; }
      const st = cls ? cls : this.styleStr();
      const last = this.segs[this.segs.length - 1];
      if (last && last.s === st) last.x += text; else this.segs.push({ x: text, s: st });
    }
    styleStr() {
      const s = this.style; let o = '';
      const fg = s.fgc || (s.fg >= 0 ? PALETTE[s.fg + (s.bold && s.fg < 8 ? 8 : 0)] : '');
      if (fg) o += `color:${fg};`;
      const bg = s.bgc || (s.bg >= 0 ? PALETTE[s.bg] : '');
      if (bg) o += `background:${bg};`;
      if (s.bold) o += 'font-weight:700;';
      return o;
    }
    drawLine() {
      if (!this.cur) return;
      let h = this.ts ? `<span class="ts">${fmtTs(this.lineT)} </span>` : '';
      for (const g of this.segs) h += g.s === 'tx' || g.s === 'cc' ? `<span class="${g.s}">${esc(g.x)}</span>` : g.s ? `<span style="${g.s}">${esc(g.x)}</span>` : esc(g.x);
      this.cur.innerHTML = h || '​';
    }
    textChunk({ b, t, tx }) {
      this.lastT = t; this.lastTx = tx;
      if (tx) {
        // Local echo: shown inline in its own colour, no escape processing.
        const s = new TextDecoder().decode(b);
        for (const ch of s) {
          if (ch === '\n') this.endLine();
          else if (ch === '\r') continue;
          else this.put(ch < ' ' ? ctl(ch) : ch, 'tx');
        }
        return;
      }
      const s = this.dec.decode(b, { stream: true });
      for (const ch of s) {
        const c = ch.charCodeAt(0);
        if (this.esc === 1) { // after ESC
          if (ch === '[') { this.esc = 2; this.csi = ''; } else if (ch === ']') this.esc = 3; else this.esc = 0;
          continue;
        }
        if (this.esc === 2) { // CSI params until a final byte
          if (c >= 0x40 && c <= 0x7e) { this.esc = 0; this.csiDone(ch); } else this.csi += ch;
          continue;
        }
        if (this.esc === 3) { // OSC until BEL or ST
          if (c === 7 || ch === '\\') this.esc = 0;
          continue;
        }
        if (c === 27) { this.esc = 1; continue; }
        if (ch === '\n') { this.cr = false; this.endLine(); continue; }
        if (ch === '\r') { this.cr = true; continue; }
        if (c === 8) { const g = this.segs[this.segs.length - 1]; if (g) { g.x = g.x.slice(0, -1); if (!g.x) this.segs.pop(); } continue; }
        if (c === 7) continue;
        if (c < 32 && c !== 9) { this.put(ctl(ch), 'cc'); continue; }
        this.put(ch);
      }
    }
    csiDone(f) {
      if (f === 'm') {
        const p = this.csi === '' ? [0] : this.csi.split(';').map(x => parseInt(x || '0', 10));
        const s = this.style;
        for (let i = 0; i < p.length; i++) {
          const n = p[i];
          if (n === 0) { s.fg = s.bg = -1; s.bold = false; s.fgc = s.bgc = ''; }
          else if (n === 1) s.bold = true;
          else if (n === 22) s.bold = false;
          else if (n >= 30 && n <= 37) { s.fg = n - 30; s.fgc = ''; }
          else if (n >= 90 && n <= 97) { s.fg = n - 90 + 8; s.fgc = ''; }
          else if (n === 39) { s.fg = -1; s.fgc = ''; }
          else if (n >= 40 && n <= 47) { s.bg = n - 40; s.bgc = ''; }
          else if (n >= 100 && n <= 107) { s.bg = n - 100 + 8; s.bgc = ''; }
          else if (n === 49) { s.bg = -1; s.bgc = ''; }
          else if (n === 38 || n === 48) {
            let col = '';
            if (p[i + 1] === 5) { col = c256(p[i + 2]); i += 2; }
            else if (p[i + 1] === 2) { col = `rgb(${p[i + 2] | 0},${p[i + 3] | 0},${p[i + 4] | 0})`; i += 4; }
            if (n === 38) s.fgc = col; else s.bgc = col;
          }
        }
      } else if (f === 'J' && (this.csi === '2' || this.csi === '3')) {
        this.screen.textContent = ''; this.cur = null; this.frag = document.createDocumentFragment();
      } else if (f === 'K') {
        if (this.cr || this.csi === '2') this.segs = [];
      }
      // Cursor movement and the rest are ignored: this is a log view, not a screen emulator.
    }
    hexChunk({ b, t, tx }) {
      if (tx !== this.hexTx && this.hexBuf.length) this.hexFlush();
      this.hexTx = tx;
      for (const x of b) {
        if (!this.hexBuf.length) this.hexT = t;
        this.hexBuf.push(x);
        if (this.hexBuf.length === 16) this.hexFlush();
      }
    }
    hexFlush() {
      this.drawHexLine();
      if (this.cur && !this.cur.isConnected) this.frag.appendChild(this.cur);
      if (!this.hexTx) this.hexOff += this.hexBuf.length;
      this.hexBuf = []; this.cur = null;
    }
    drawHexLine() {
      if (!this.hexBuf.length) return;
      if (!this.cur) this.cur = document.createElement('div');
      const bs = this.hexBuf;
      const hx = bs.map(x => x.toString(16).padStart(2, '0')).join(' ').padEnd(47, ' ');
      const asc = bs.map(x => x >= 32 && x < 127 ? String.fromCharCode(x) : '.').join('');
      const lead = this.hexTx ? '  sent  ' : this.hexOff.toString(16).padStart(8, '0');
      this.cur.innerHTML = `${this.ts ? `<span class="ts">${fmtTs(this.hexT)} </span>` : ''}<span class="hx-off">${lead}</span>  <span class="${this.hexTx ? 'tx' : ''}">${hx}  |${esc(asc)}|</span>`;
    }
  }

  function ctl(ch) { const c = ch.charCodeAt(0); return c === 127 ? '␡' : String.fromCharCode(0x2400 + c); }
  function c256(n) {
    if (n < 16) return PALETTE[n];
    if (n >= 232) { const v = 8 + (n - 232) * 10; return `rgb(${v},${v},${v})`; }
    n -= 16; const k = [0, 95, 135, 175, 215, 255];
    return `rgb(${k[Math.floor(n / 36)]},${k[Math.floor(n / 6) % 6]},${k[n % 6]})`;
  }
  function fmtTs(t) { const d = new Date(t || Date.now()); return d.toTimeString().slice(0, 8) + '.' + String(d.getMilliseconds()).padStart(3, '0'); }
  function fmtN(n) { return n < 1024 ? n + ' B' : n < 1048576 ? (n / 1024).toFixed(1) + ' KB' : (n / 1048576).toFixed(1) + ' MB'; }
  function b64(s) { const bin = atob(s); const u = new Uint8Array(bin.length); for (let i = 0; i < bin.length; i++) u[i] = bin.charCodeAt(i); return u; }
  const enc = new TextEncoder();
  const port = id => ST?.ports?.find(p => p.id === id);
  const api = (id, what, body) => papi('POST', `/api/serial/${encodeURIComponent(id)}/${what}`, body);

  // ---------- Section ----------

  function mount(el, st) {
    root = el;
    el.innerHTML = `<div class="ser-list"></div>
      <div class="pr-row" style="margin-top:12px"><label>Add a tty by path <input class="ser-manual" placeholder="/dev/pts/3" style="width:150px"></label><button class="btn btn-sm" data-act="manual">Add</button><span class="pr-note">For ptys, socat links and other ports udev doesn't list as USB serial</span></div>
      <div class="pr-row ser-ifaces"></div>`;
    el.addEventListener('click', onClick);
    el.addEventListener('change', onChange);
    el.addEventListener('keydown', onKey);
    update(st);
  }

  function unmount() { closeTerm(); root = null; }

  function update(st) {
    ST = st;
    if (!root) return;
    const list = root.querySelector('.ser-list');
    const ids = new Set(st.ports.map(p => p.id));
    for (const c of [...list.children]) if (!ids.has(c.dataset.id)) { if (term?.id === c.dataset.id) closeTerm(); c.remove(); }
    if (!st.ports.length && !list.querySelector('.empty')) list.innerHTML = '<div class="empty">No serial ports<div class="empty-sub">Plug in a USB-serial adapter or dev board</div></div>';
    if (st.ports.length) list.querySelector('.empty')?.remove();
    const ib = window.gpioIfaceBtn?.('uart', 'Onboard UARTs…') || '';
    const ifs = root.querySelector('.ser-ifaces');
    if (ifs.dataset.k !== ib) { ifs.dataset.k = ib; ifs.innerHTML = ib && `${ib}<span class="pr-note">Turn on the header UART or extra ones (UART2–5 on a Pi 4); config.txt, needs a reboot</span>`; }
    for (const p of st.ports) {
      let card = list.querySelector(`.ser-card[data-id="${CSS.escape(p.id)}"]`);
      if (!card) {
        card = document.createElement('div');
        card.className = 'pr-card ser-card'; card.dataset.id = p.id;
        card.innerHTML = '<div class="ser-head"></div><div class="ser-body"></div>';
        list.appendChild(card);
      }
      renderHead(card, p);
      if (selected === p.id && p.present) { if (term?.id !== p.id) openTerm(card, p); else renderPanels(p); }
      else if (term?.id === p.id) closeTerm();
    }
  }

  function renderHead(card, p) {
    card.classList.toggle('gone', !p.present);
    const pill = !p.present ? '<span class="pr-pill">unplugged</span>'
      : p.state === 'flashing' ? `<span class="pr-pill warn">flashing ${p.flash?.progress ?? 0}%</span>`
      : p.state === 'autobaud' ? '<span class="pr-pill warn">auto-baud…</span>'
      : p.open ? '<span class="pr-pill on">open</span>' : '<span class="pr-pill">closed</span>';
    const clients = p.clients.ws + p.clients.tcp + p.clients.rfc2217;
    const br = ['tcp', 'rfc2217'].filter(k => p.bridges[k].enabled).map(k => `${k === 'tcp' ? 'TCP' : 'RFC 2217'} :${p.bridges[k].port}${p.bridges[k].error ? ' (failed)' : ''}`);
    const meta = [p.path, p.byId && p.byId !== p.path ? p.byId.replace('/dev/serial/by-id/', 'by-id/') : '', p.vidpid, p.usbDriver, p.serial && 'S/N ' + p.serial].filter(Boolean);
    const sig = JSON.stringify([p.present, p.state, p.open, p.flash?.progress, p.configLabel, p.rxBytes, p.txBytes, clients, br, p.error, p.lock, selected === p.id, p.console]);
    if (card.dataset.sig === sig) return;
    card.dataset.sig = sig;
    const isSel = selected === p.id;
    card.querySelector('.ser-head').innerHTML = `
      <h4>${esc(p.name)} ${pill}${p.console ? '<span class="pr-pill warn">console · read-only</span>' : ''}${p.lock ? `<span class="pr-pill warn">locked by ${esc(p.lock.owner)}</span>` : ''}</h4>
      <div class="ser-meta">${meta.map(m => `<span class="mono">${esc(m)}</span>`).join('')}</div>
      <div class="pr-row">
        ${p.present ? (p.open ? `<button class="btn btn-sm" data-act="close">Close</button>` : `<button class="btn btn-sm btn-primary" data-act="open" ${p.state === 'flashing' ? 'disabled' : ''}>Open</button>`) : ''}
        ${p.present ? `<button class="btn btn-sm" data-act="term">${isSel ? 'Hide terminal' : 'Terminal'}</button>` : ''}
        <span class="pr-note">${esc(p.configLabel)} · rx ${fmtN(p.rxBytes)} · tx ${fmtN(p.txBytes)}${clients ? ` · ${clients} client${clients > 1 ? 's' : ''}` : ''}${br.length ? ' · ' + esc(br.join(', ')) : ''}</span>
      </div>
      ${p.error ? `<div class="pr-note" style="color:var(--danger)">${esc(p.error)}</div>` : ''}`;
  }

  // ---------- Terminal panel ----------

  function openTerm(card, p) {
    closeTerm();
    const body = card.querySelector('.ser-body');
    const id = p.id;
    const ro = p.console;
    body.innerHTML = `
      <div class="pr-row ser-cfg">
        <label>Baud <input class="ser-baud" list="ser-bauds" inputmode="numeric" style="width:96px"></label>
        <datalist id="ser-bauds">${BAUDS.map(b => `<option value="${b}">`).join('')}</datalist>
        <label>Bits <select class="ser-bits">${[8, 7, 6, 5].map(n => `<option>${n}</option>`).join('')}</select></label>
        <label>Parity <select class="ser-par">${['none', 'even', 'odd', 'mark', 'space'].map(n => `<option>${n}</option>`).join('')}</select></label>
        <label>Stop <select class="ser-stop"><option>1</option><option>2</option></select></label>
        <label>Flow <select class="ser-flow"><option value="none">none</option><option value="rtscts">RTS/CTS</option><option value="xonxoff">XON/XOFF</option></select></label>
        <button class="btn btn-sm" data-act="apply" ${ro ? 'disabled' : ''}>Apply</button>
        <button class="btn btn-sm" data-act="autobaud" ${ro ? 'disabled' : ''}>Auto-baud</button>
        <span class="pr-note ser-ab"></span>
      </div>
      <div class="pr-row ser-lines"></div>
      <div class="pr-row">
        <select class="ser-view"><option value="text">Text</option><option value="hex">Hex</option></select>
        <label><input type="checkbox" class="ser-ts"> Timestamps</label>
        <label><input type="checkbox" class="ser-echo" checked> Local echo</label>
        <label>Line end <select class="ser-eol"><option value="lf">LF</option><option value="cr">CR</option><option value="crlf">CR+LF</option><option value="none">none</option></select></label>
        <button class="btn btn-sm ser-tog" data-act="raw" ${ro ? 'disabled' : ''}>Raw keys</button>
        <button class="btn btn-sm" data-act="clear">Clear</button>
        <a class="btn btn-sm" href="/api/serial/${encodeURIComponent(id)}/read?format=raw&download=1" download>Download log</a>
      </div>
      <div class="pr-note ser-rawnote" style="display:none">Raw keys: every keystroke, including Ctrl-C, goes to the device. Click outside the terminal or press the button again to stop.</div>
      <div class="ser-screen" tabindex="0"></div>
      <div class="pr-row ser-in">
        <input class="ser-line" placeholder="${ro ? 'Console port: read-only' : 'Type and press Enter'}" ${ro ? 'disabled' : ''} autocomplete="off" autocapitalize="off" spellcheck="false">
        <label><input type="checkbox" class="ser-hexin"> Hex</label>
        <button class="btn btn-sm btn-primary" data-act="send" ${ro ? 'disabled' : ''}>Send</button>
      </div>
      <details class="ser-more ser-br"><summary>Network bridges</summary><div class="ser-br-body"></div></details>
      ${p.flashTools.length ? `<details class="ser-more ser-fl"><summary>Flash firmware</summary><div class="ser-fl-form"></div><div class="ser-fl-prog"></div></details>` : ''}
      <details class="ser-more ser-opt"><summary>Port options</summary><div class="ser-opt-body"></div></details>`;
    const screen = body.querySelector('.ser-screen');
    const pref = JSON.parse(localStorage.getItem('serialView') || '{}');
    const t = new Term(screen);
    t.hex = pref.view === 'hex'; t.ts = !!pref.ts; t.echo = pref.echo !== false;
    body.querySelector('.ser-view').value = t.hex ? 'hex' : 'text';
    body.querySelector('.ser-ts').checked = t.ts;
    body.querySelector('.ser-echo').checked = t.echo;
    body.querySelector('.ser-eol').value = pref.eol || 'lf';
    term = { id, body, t, raw: false, cfgShown: '', ws: null, flashLog: [] };
    fillCfg(p, true);
    renderFlashForm(p);
    renderPanels(p);
    screen.addEventListener('keydown', rawKey);
    screen.addEventListener('paste', e => { if (!term?.raw) return; e.preventDefault(); sendBytes(enc.encode(e.clipboardData.getData('text'))); });
    screen.addEventListener('blur', () => term?.raw && setRaw(false));
    term.ws = pws(`/ws/serial/${encodeURIComponent(id)}?history=262144&ts=1`, { onmessage: onWs });
  }

  function closeTerm() {
    if (!term) return;
    term.ws?.close();
    if (term.t.raf) cancelAnimationFrame(term.t.raf);
    term.body.innerHTML = '';
    term = null;
  }

  function onWs(m) {
    if (!term) return;
    if (m instanceof ArrayBuffer) return term.t.feed(new Uint8Array(m), Date.now(), false);
    if (!m || typeof m !== 'object') return;
    if (m.type === 'hello') { term.t.clear(); patchPort(m.port); }
    else if (m.type === 'history') for (const [t, d] of m.chunks) term.t.feed(b64(d), t, false);
    else if (m.type === 'status') patchPort(m.port);
    else if (m.type === 'lines') { const p = port(term.id); if (p) { p.lines = m.lines; renderLines(p); } }
    else if (m.type === 'flash') { if (m.line) { term.flashLog.push(m.line); if (term.flashLog.length > 300) term.flashLog.shift(); } const p = port(term.id); if (p) { p.flash = m.job; renderFlashProg(p); } }
    else if (m.type === 'error') toast(m.message, 'error');
  }

  function patchPort(np) {
    if (!ST || !np) return;
    const i = ST.ports.findIndex(p => p.id === np.id);
    if (i >= 0) ST.ports[i] = np; else ST.ports.push(np);
    update(ST);
  }

  function renderPanels(p) {
    if (!term) return;
    fillCfg(p, false);
    renderLines(p);
    renderBridges(p);
    renderFlashProg(p);
    renderOptions(p);
    const ab = term.body.querySelector('.ser-ab');
    const a = p.autobaud;
    const abTxt = p.state === 'autobaud' ? 'listening…' : a ? (a.best ? `best guess ${a.best.baud} (confidence ${Math.round(a.best.confidence * 100)}%)${a.applied ? ', applied' : ''}` : 'no data heard; the device must be sending') : '';
    if (ab.textContent !== abTxt) ab.textContent = abTxt;
  }

  function fillCfg(p, force) {
    const b = term.body, c = p.config;
    const sig = JSON.stringify(c);
    if (!force && term.cfgShown === sig) return;
    // Don't clobber a field the user is editing.
    if (!force && b.querySelector('.ser-cfg').contains(document.activeElement)) return;
    term.cfgShown = sig;
    b.querySelector('.ser-baud').value = c.baud;
    b.querySelector('.ser-bits').value = c.dataBits;
    b.querySelector('.ser-par').value = c.parity;
    b.querySelector('.ser-stop').value = c.stopBits;
    b.querySelector('.ser-flow').value = c.flow;
    b.querySelectorAll('.ser-cfg select, .ser-cfg input').forEach(e => e.disabled = p.console);
  }

  function renderLines(p) {
    const el = term.body.querySelector('.ser-lines');
    const l = p.lines;
    const sig = JSON.stringify([l, p.open, p.resets, p.console, p.state]);
    if (el.dataset.sig === sig) return;
    el.dataset.sig = sig;
    if (!p.open) { el.innerHTML = '<span class="pr-note">Open the port for modem lines and resets.</span>'; return; }
    const lamp = (k, label) => `<span class="ser-lamp ${l?.[k] ? 'on' : ''}"><i></i>${label}</span>`;
    const tog = (k, label) => `<button class="btn btn-sm ser-tog ${l?.[k] ? 'on' : ''}" data-act="line" data-line="${k}" ${!l || p.console ? 'disabled' : ''}>${label}</button>`;
    el.innerHTML = (l ? `${tog('dtr', 'DTR')}${tog('rts', 'RTS')}${lamp('cts', 'CTS')}${lamp('dsr', 'DSR')}${lamp('dcd', 'DCD')}${lamp('ri', 'RI')}` : '<span class="pr-note">No modem lines on this port</span>')
      + (p.console ? '' : `<span class="ser-sep"></span><button class="btn btn-sm" data-act="break">Break</button>`)
      + (p.resets.length ? `<span class="ser-sep"></span>${p.resets.map(r => `<button class="btn btn-sm btn-warn" data-act="reset" data-mode="${r}" ${!l && r !== 'touch1200' ? 'disabled' : ''}>${esc(RESET_LABELS[r])}</button>`).join('')}` : '');
  }

  function renderBridges(p) {
    const el = term.body.querySelector('.ser-br-body');
    const host = location.hostname;
    const sig = JSON.stringify([p.bridges, p.config.baud, host]);
    if (el.dataset.sig === sig) return;
    el.dataset.sig = sig;
    const wsUrl = `${location.protocol === 'https:' ? 'wss' : 'ws'}://${location.host}/ws/serial/${p.id}`;
    const block = (k, title, about, cmds) => {
      const b = p.bridges[k];
      const st = b.error ? `<span class="pr-pill bad">${esc(b.error)}</span>` : b.listening ? `<span class="pr-pill on">listening on :${b.port}</span>${b.clients.length ? ` <span class="pr-note">${b.clients.length} connected: ${esc(b.clients.join(', '))}</span>` : ''}` : '<span class="pr-pill">off</span>';
      return `<div class="pr-row"><label><input type="checkbox" data-bridge="${k}" ${b.enabled ? 'checked' : ''}> <b>${title}</b></label>
        <label>port <input type="number" class="ser-bport" data-kind="${k}" value="${b.port}" min="1024" max="65535" style="width:84px"></label>
        <button class="btn btn-sm" data-act="bport" data-kind="${k}">Set port</button> ${st}</div>
        <div class="pr-note">${about}</div>
        ${b.enabled ? `<div class="ser-cmds">${cmds(b.port).map(c => `<span class="pr-endpoint">${esc(c)}</span>`).join('')}</div>` : ''}`;
    };
    el.innerHTML =
      block('tcp', 'Raw TCP', 'Plain bytes both ways. Line settings stay as set here.', port => [`nc ${host} ${port}`, `socat -,raw,echo=0 TCP:${host}:${port}`, `socat pty,link=/tmp/ttyDesky,raw,echo=0 TCP:${host}:${port}`])
      + block('rfc2217', 'RFC 2217', 'Telnet COM-PORT-OPTION: remote tools can change baud, framing, DTR/RTS and send breaks. Works with pyserial and esptool.', port => [`python -m serial.tools.miniterm rfc2217://${host}:${port} ${p.config.baud}`, `esptool --port 'rfc2217://${host}:${port}?ign_set_control' flash_id`, `pio device monitor --port rfc2217://${host}:${port} -b ${p.config.baud}`])
      + `<div class="pr-row"><b>WebSocket</b> <span class="pr-note">always on: binary frames are raw bytes; quiet=1 drops the JSON status frames, open=1 holds the port open while connected</span></div>
        <div class="ser-cmds"><span class="pr-endpoint">${esc(`websocat --binary '${wsUrl}?quiet=1&open=1'`)}</span></div>`;
  }

  function renderFlashForm(p) {
    const el = term.body.querySelector('.ser-fl-form');
    if (!el) return;
    const tools = p.flashTools;
    el.innerHTML = `<div class="pr-row">
        <label>Tool <select class="ser-fl-tool">${tools.map(t => `<option>${t}</option>`).join('')}</select></label>
        <label>Action <select class="ser-fl-act"><option value="write">Write</option><option value="info">Info</option><option value="erase">Erase</option></select></label>
        <span class="ser-fl-esp"><label>Chip <select class="ser-fl-chip">${['auto', 'esp32', 'esp32s2', 'esp32s3', 'esp32c2', 'esp32c3', 'esp32c6', 'esp32h2', 'esp32p4', 'esp8266'].map(c => `<option>${c}</option>`).join('')}</select></label>
        <label>Baud <select class="ser-fl-baud"><option>115200</option><option selected>460800</option><option>921600</option></select></label></span>
        <span class="ser-fl-avr"><label>Board <select class="ser-fl-preset"><option value="uno">Uno (115200)</option><option value="nano">Nano (115200)</option><option value="nano-old">Nano, old bootloader (57600)</option><option value="mega">Mega 2560 (wiring)</option></select></label></span>
      </div>
      <div class="ser-files"></div>
      <div class="pr-row"><button class="btn btn-sm ser-fl-add" data-act="fladd">Add file</button><button class="btn btn-sm btn-primary" data-act="flash">Start</button><button class="btn btn-sm btn-danger" data-act="flcancel">Cancel</button></div>
      <div class="pr-note">DeskyPi releases the port while the tool runs and reopens it after. ESP32: bootloader 0x1000, partition table 0x8000, app 0x10000 (ESP32-S3/C3/C6 bootloader at 0x0), or one merged image at 0x0.</div>`;
    addFileRow('0x0');
    syncFlashForm();
  }

  function addFileRow(off) {
    const box = term.body.querySelector('.ser-files');
    const d = document.createElement('div');
    d.className = 'pr-row';
    d.innerHTML = `<input type="file" class="ser-fl-file" accept=".bin,.hex,.img"><label class="ser-fl-offl">offset <input class="ser-fl-off" value="${esc(off)}" style="width:90px"></label>`;
    box.appendChild(d);
  }

  function syncFlashForm() {
    const b = term?.body; if (!b?.querySelector('.ser-fl-form')) return;
    const tool = b.querySelector('.ser-fl-tool').value;
    const act = b.querySelector('.ser-fl-act').value;
    b.querySelector('.ser-fl-esp').style.display = tool === 'esptool' ? '' : 'none';
    b.querySelector('.ser-fl-avr').style.display = tool === 'avrdude' ? '' : 'none';
    b.querySelector('.ser-files').style.display = act === 'write' ? '' : 'none';
    b.querySelector('.ser-fl-add').style.display = act === 'write' && tool === 'esptool' ? '' : 'none';
    b.querySelectorAll('.ser-fl-offl').forEach(e => e.style.display = tool === 'esptool' ? '' : 'none');
    b.querySelector('.ser-fl-act option[value=erase]').disabled = tool === 'avrdude';
  }

  function renderFlashProg(p) {
    const el = term?.body.querySelector('.ser-fl-prog');
    if (!el) return;
    const j = p.flash;
    if (!j) { el.innerHTML = ''; return; }
    const cls = j.state === 'done' ? 'done' : j.state === 'error' || j.state === 'cancelled' ? 'error' : 'active';
    const log = term.flashLog.length ? term.flashLog : j.logTail || [];
    el.innerHTML = `<div class="pr-row"><b>${esc(j.tool)} ${esc(j.action)}</b><span class="pr-note">${esc(j.label)}</span><span class="pr-pill ${cls === 'done' ? 'on' : cls === 'error' ? 'bad' : 'warn'}">${esc(j.state)}${j.state === 'running' ? ' · ' + esc(j.phase) : ''}</span><span class="pr-note">${j.progress}%</span></div>
      ${j.message ? `<div class="pr-note" style="color:${cls === 'error' ? 'var(--danger)' : 'var(--text)'}">${esc(j.message)}</div>` : ''}
      <div class="op-bar"><div class="op-fill ${cls}" style="width:${j.progress}%"></div></div>
      <div class="pr-log" style="max-height:160px;margin-top:6px">${esc(log.slice(-60).join('\n'))}</div>`;
    const lg = el.querySelector('.pr-log'); lg.scrollTop = lg.scrollHeight;
  }

  function renderOptions(p) {
    const el = term.body.querySelector('.ser-opt-body');
    const mine = !!term.locked;
    const sig = JSON.stringify([p.openOnPlug, p.lock, mine, p.console]);
    if (el.dataset.sig === sig) return;
    el.dataset.sig = sig;
    el.innerHTML = `<div class="pr-row"><label><input type="checkbox" data-opt="openOnPlug" ${p.openOnPlug ? 'checked' : ''}> Open automatically when plugged in (and when DeskyPi starts)</label></div>
      ${p.console ? '' : `<div class="pr-row">${p.lock ? (mine ? '<span class="pr-pill on">You hold the write lock</span><button class="btn btn-sm" data-act="unlock">Release</button>' : `<span class="pr-pill warn">Writing locked by ${esc(p.lock.owner)}</span><button class="btn btn-sm btn-danger" data-act="breaklock">Break lock</button>`) : '<button class="btn btn-sm" data-act="lock">Lock writing to this tab</button><span class="pr-note">Other clients (bridges, agents, other tabs) can still read but not write</span>'}</div>`}
      <div class="pr-note">Port id <span class="pr-endpoint">${esc(p.id)}</span> for the API and MCP tools (serial_write, serial_read…).</div>`;
  }

  // ---------- Actions ----------

  function sendBytes(u8) {
    if (!term) return;
    const p = port(term.id);
    if (!p?.open) { toast('Open the port first', 'error'); return; }
    if (term.ws?.ready) term.ws.send(u8);
    else api(term.id, 'write', { base64: btoa(String.fromCharCode(...u8)) }).catch(() => {});
    term.t.feed(u8, Date.now(), true);
  }

  function sendLine() {
    const inp = term.body.querySelector('.ser-line');
    const eol = { none: '', cr: '\r', lf: '\n', crlf: '\r\n' }[term.body.querySelector('.ser-eol').value];
    let bytes;
    if (term.body.querySelector('.ser-hexin').checked) {
      const h = inp.value.replace(/0x/gi, '').replace(/[\s,:]/g, '');
      if (!/^([0-9a-f]{2})*$/i.test(h)) { toast('Hex must be pairs of hex digits', 'error'); return; }
      bytes = new Uint8Array((h.match(/../g) || []).map(x => parseInt(x, 16)));
    } else bytes = enc.encode(inp.value + eol);
    if (!bytes.length) return;
    sendBytes(bytes);
    inp.value = '';
  }

  function setRaw(on) {
    term.raw = on;
    const sc = term.body.querySelector('.ser-screen');
    sc.classList.toggle('raw', on);
    term.body.querySelector('[data-act=raw]').classList.toggle('on', on);
    term.body.querySelector('.ser-rawnote').style.display = on ? '' : 'none';
    if (on) sc.focus();
  }

  const KEYS = { Enter: null, Backspace: '\x7f', Tab: '\t', Escape: '\x1b', ArrowUp: '\x1b[A', ArrowDown: '\x1b[B', ArrowRight: '\x1b[C', ArrowLeft: '\x1b[D', Home: '\x1b[H', End: '\x1b[F', Delete: '\x1b[3~', PageUp: '\x1b[5~', PageDown: '\x1b[6~', Insert: '\x1b[2~' };
  function rawKey(e) {
    if (!term?.raw) return;
    if (e.metaKey || (e.ctrlKey && e.shiftKey)) return; // leave Cmd/Ctrl-Shift shortcuts (copy/paste) to the browser
    let s = null;
    if (e.key === 'Enter') s = { none: '\r', cr: '\r', lf: '\n', crlf: '\r\n' }[term.body.querySelector('.ser-eol').value];
    else if (e.key in KEYS) s = KEYS[e.key];
    else if (e.ctrlKey && e.key.length === 1) { const c = e.key.toUpperCase().charCodeAt(0); if (c >= 64 && c <= 95) s = String.fromCharCode(c - 64); }
    else if (e.key.length === 1 && !e.altKey) s = e.key;
    if (s == null) return;
    e.preventDefault();
    const u8 = enc.encode(s);
    // Raw mode is for interactive shells, which echo themselves: skip local echo.
    if (term.ws?.ready) term.ws.send(u8);
  }

  async function startFlash() {
    const b = term.body, id = term.id;
    const tool = b.querySelector('.ser-fl-tool').value, action = b.querySelector('.ser-fl-act').value;
    const fd = new FormData();
    fd.append('tool', tool); fd.append('action', action);
    if (tool === 'esptool') { fd.append('chip', b.querySelector('.ser-fl-chip').value); fd.append('baud', b.querySelector('.ser-fl-baud').value); }
    else fd.append('preset', b.querySelector('.ser-fl-preset').value);
    if (action === 'write') {
      let n = 0;
      for (const row of b.querySelectorAll('.ser-files .pr-row')) {
        const f = row.querySelector('.ser-fl-file').files[0];
        if (!f) continue;
        fd.append('offset', row.querySelector('.ser-fl-off').value || '0');
        fd.append('file', f, f.name);
        n++;
      }
      if (!n) { toast('Choose a firmware file', 'error'); return; }
    }
    term.flashLog = [];
    try {
      const r = await fetch(`/api/serial/${encodeURIComponent(id)}/flash`, { method: 'POST', body: fd });
      const j = await r.json().catch(() => ({}));
      if (!r.ok || j.ok === false) toast(j.message || r.statusText, 'error');
    } catch (e) { toast('Upload failed: ' + e.message, 'error'); }
  }

  function onClick(e) {
    const el = e.target.closest('[data-act]');
    if (!el) return;
    const card = el.closest('.ser-card');
    const id = card?.dataset.id;
    const p = id && port(id);
    const act = el.dataset.act;
    const quiet = () => {};
    switch (act) {
      case 'manual': {
        const inp = root.querySelector('.ser-manual');
        if (inp.value.trim()) papi('POST', '/api/serial/manual', { path: inp.value.trim() }).then(r => { inp.value = ''; selected = r.id; localStorage.setItem('serialTerm', selected); }).catch(quiet);
        break;
      }
      case 'open': api(id, 'open').catch(quiet); break;
      case 'close': api(id, 'close').catch(quiet); break;
      case 'term':
        selected = selected === id ? '' : id;
        localStorage.setItem('serialTerm', selected);
        card.dataset.sig = '';
        update(ST);
        break;
      case 'apply': {
        const b = term.body;
        api(id, 'config', { baud: Number(b.querySelector('.ser-baud').value), dataBits: Number(b.querySelector('.ser-bits').value), parity: b.querySelector('.ser-par').value, stopBits: Number(b.querySelector('.ser-stop').value), flow: b.querySelector('.ser-flow').value }).then(() => toast('Line settings applied', 'success')).catch(quiet);
        break;
      }
      case 'autobaud':
        api(id, 'autobaud', { apply: true }).then(r => toast(r.best ? `Best guess ${r.best.baud} baud (${Math.round(r.best.confidence * 100)}%)${r.applied ? ', applied' : ', not applied (low confidence)'}` : 'Heard nothing at any rate', r.applied ? 'success' : 'info')).catch(quiet);
        break;
      case 'line': { const k = el.dataset.line; api(id, 'lines', { [k]: !p?.lines?.[k] }).catch(quiet); break; }
      case 'break': api(id, 'break', { ms: 250 }).catch(quiet); break;
      case 'reset': api(id, 'reset', { mode: el.dataset.mode }).catch(quiet); break;
      case 'raw': setRaw(!term.raw); break;
      case 'clear': term.t.clear(); break;
      case 'send': sendLine(); break;
      case 'bport': {
        const k = el.dataset.kind;
        const v = Number(term.body.querySelector(`.ser-bport[data-kind="${k}"]`).value);
        api(id, 'bridge', { kind: k, enabled: !!p.bridges[k].enabled, port: v }).catch(quiet);
        break;
      }
      case 'fladd': addFileRow('0x10000'); break;
      case 'flash': startFlash(); break;
      case 'flcancel': api(id, 'flash/cancel').catch(quiet); break;
      case 'lock': term.ws?.send({ type: 'lock', owner: 'a dashboard tab' }); term.locked = true; break;
      case 'unlock': term.ws?.send({ type: 'unlock' }); term.locked = false; break;
      case 'breaklock': api(id, 'unlock', { force: true }).catch(quiet); break;
    }
  }

  function onChange(e) {
    const t = e.target;
    const id = t.closest('.ser-card')?.dataset.id;
    if (!id || !term) return;
    if (t.dataset.bridge) api(id, 'bridge', { kind: t.dataset.bridge, enabled: t.checked }).catch(() => { t.checked = !t.checked; });
    else if (t.dataset.opt === 'openOnPlug') api(id, 'settings', { openOnPlug: t.checked }).catch(() => {});
    else if (t.matches('.ser-view, .ser-ts, .ser-echo, .ser-eol')) {
      const b = term.body;
      term.t.hex = b.querySelector('.ser-view').value === 'hex';
      term.t.ts = b.querySelector('.ser-ts').checked;
      term.t.echo = b.querySelector('.ser-echo').checked;
      localStorage.setItem('serialView', JSON.stringify({ view: term.t.hex ? 'hex' : 'text', ts: term.t.ts, echo: term.t.echo, eol: b.querySelector('.ser-eol').value }));
      if (!t.matches('.ser-eol')) term.t.rerender();
    } else if (t.matches('.ser-fl-tool, .ser-fl-act')) syncFlashForm();
  }

  function onKey(e) {
    if (e.key === 'Enter' && e.target.matches('.ser-line')) { e.preventDefault(); sendLine(); }
    else if (e.key === 'Enter' && e.target.matches('.ser-baud')) { e.preventDefault(); term.body.querySelector('[data-act=apply]').click(); }
  }

  periphRegister('serial', { mount, update, unmount });
})();
