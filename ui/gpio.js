// GPIO panel: live 40-pin header (or table) over /ws/gpio, opt-in output control, and the
// config.txt interface switches (stage → diff → confirm → backup + write → reboot confirm).
// The WebSocket only exists while this section is open, so the server only polls then.
(() => {
  let root = null, sock = null, pins = [], st = null, view = 'header', live = 0, err = '';
  try { view = localStorage.getItem('gpioView') || 'header'; } catch {}

  const CSS = `
.gp-hdr{display:grid;grid-template-columns:1fr auto auto 1fr;gap:3px 6px;align-items:center;max-width:640px;margin:8px auto 0}
.gp-l{text-align:right;font-size:.74rem;line-height:1.2;min-width:0;overflow-wrap:anywhere}
.gp-r{text-align:left;font-size:.74rem;line-height:1.2;min-width:0;overflow-wrap:anywhere}
.gp-l small,.gp-r small{display:block;color:var(--dim);font-size:.66rem}
.gp-pin{width:26px;height:26px;border-radius:50%;display:flex;align-items:center;justify-content:center;font-size:.68rem;font-weight:700;border:2px solid transparent;color:#fff;position:relative;font-family:inherit;padding:0}
.gp-pin.click{cursor:pointer}
.gp-pin.click:hover{outline:2px solid var(--accent)}
.gp-3v3{background:#b5651d}.gp-5v{background:#b22222}.gp-gnd{background:#2b2b2b;color:#999}
.gp-in{background:#2f4f8f}.gp-out{background:#2e7d4f}.gp-alt{background:#6a3f9a}.gp-off{background:#444}
.gp-claimed{border-color:var(--warning)}
.gp-dot{width:8px;height:8px;border-radius:50%;display:inline-block;vertical-align:middle;margin:0 4px;background:#333;border:1px solid #555}
.gp-dot.hi{background:var(--success);border-color:var(--success)}
.gp-key{display:flex;flex-wrap:wrap;gap:4px 12px;font-size:.7rem;color:var(--dim);margin-top:8px}
.gp-key span{display:inline-flex;align-items:center;gap:4px}
.gp-key i{width:10px;height:10px;border-radius:50%;display:inline-block}
.gp-diff{white-space:pre;overflow:auto;max-height:50vh}
.gp-diff .a{color:var(--success)}.gp-diff .d{color:var(--danger)}.gp-diff .h{color:var(--accent)}
@media (max-width:520px){.gp-hdr{gap:2px 4px}.gp-l,.gp-r{font-size:.68rem}.gp-pin{width:22px;height:22px;font-size:.6rem}}`;

  function ensureCss() {
    if (document.getElementById('gp-css')) return;
    const s = document.createElement('style');
    s.id = 'gp-css'; s.textContent = CSS;
    document.head.appendChild(s);
  }

  const byGpio = () => new Map(pins.map(p => [p.gpio, p]));
  const cls = p => !p ? 'gp-off' : p.fsel === 'op' ? 'gp-out' : p.fsel === 'ip' ? 'gp-in' : /^a\d/.test(p.fsel) ? 'gp-alt' : 'gp-off';
  const funcText = p => p.fsel === 'ip' ? 'input' : p.fsel === 'op' ? 'output' : `${p.func} (${p.fsel.toUpperCase()})`;
  const pullText = p => p.pull && p.pull !== 'none' ? ` · pull-${p.pull}` : '';
  const dot = p => p && p.level != null ? `<span class="gp-dot${p.level ? ' hi' : ''}"></span>` : '';
  const canDrive = p => st?.outputEnabled && p && !p.refusal;

  function label(h, p, side) {
    if (h.kind !== 'gpio') return `<div class="gp-${side}">${h.kind === 'gnd' ? 'GND' : h.kind === '3v3' ? '3.3 V' : '5 V'}</div>`;
    if (!p) return `<div class="gp-${side}">GPIO${h.gpio}</div>`;
    const name = `GPIO${h.gpio}`;
    const main = side === 'l' ? `${funcText(p)} ${dot(p)}<b>${name}</b>` : `<b>${name}</b>${dot(p)} ${funcText(p)}`;
    const sub = [p.consumer ? `used by ${p.consumer}` : '', pullText(p).slice(3)].filter(Boolean).join(' · ');
    return `<div class="gp-${side}">${main}${sub ? `<small>${esc(sub)}</small>` : ''}</div>`;
  }

  function pinDot(h, p) {
    const k = h.kind === 'gpio' ? cls(p) : 'gp-' + h.kind;
    const drive = h.kind === 'gpio' && canDrive(p);
    return `<button class="gp-pin ${k}${p?.consumer || p?.refusal?.startsWith('muxed') ? ' gp-claimed' : ''}${drive ? ' click' : ''}" ${drive ? `data-act="pin" data-g="${h.gpio}"` : 'tabindex="-1"'} aria-label="Pin ${h.pin}">${h.pin}</button>`;
  }

  function headerHtml(header) {
    const m = byGpio();
    let h = '<div class="gp-hdr">';
    for (let i = 0; i < 40; i += 2) {
      const a = header[i], b = header[i + 1];
      h += label(a, m.get(a.gpio), 'l') + pinDot(a, m.get(a.gpio)) + pinDot(b, m.get(b.gpio)) + label(b, m.get(b.gpio), 'r');
    }
    h += '</div><div class="gp-key"><span><i class="gp-in"></i>input</span><span><i class="gp-out"></i>output</span><span><i class="gp-alt"></i>peripheral (ALT)</span><span><i style="border:2px solid var(--warning)"></i>claimed</span><span><span class="gp-dot hi" style="margin:0"></span>high</span><span><i class="gp-3v3"></i>3.3 V</span><span><i class="gp-5v"></i>5 V</span><span><i class="gp-gnd"></i>GND</span></div>';
    return h;
  }

  function tableHtml(list) {
    return `<div style="overflow-x:auto"><table class="pr-tbl"><tr><th>GPIO</th><th>Pin</th><th>Function</th><th>Level</th><th>Pull</th><th>Used by</th>${st?.outputEnabled ? '<th></th>' : ''}</tr>${list.map(p => `<tr>
      <td>${p.gpio}</td><td>${p.header ?? '—'}</td><td>${esc(funcText(p))}</td><td>${p.level == null ? '—' : dot(p) + (p.level ? 'high' : 'low')}</td><td>${esc(p.pull || '—')}</td>
      <td>${esc(p.consumer || '')}${p.refusal && !p.consumer ? `<span class="pr-note">${esc(p.refusal)}</span>` : ''}</td>
      ${st?.outputEnabled ? `<td>${canDrive(p) ? `<button class="btn btn-sm" data-act="pin" data-g="${p.gpio}">Set…</button>` : ''}</td>` : ''}</tr>`).join('')}</table></div>`;
  }

  function render() {
    if (!root || !st) return;
    const header = st.header || [];
    const bank = pins.filter(p => p.gpio <= 27), soc = pins.filter(p => p.gpio > 27);
    const socOpen = root.querySelector('#gp-soc')?.open;
    root.innerHTML = `
      <div class="pr-row">
        <button class="btn btn-sm${view === 'header' ? ' btn-primary' : ''}" data-act="view" data-v="header">Header</button>
        <button class="btn btn-sm${view === 'table' ? ' btn-primary' : ''}" data-act="view" data-v="table">Table</button>
        <span class="pr-pill ${live && Date.now() - live < 4000 ? 'on' : 'warn'}">${live && Date.now() - live < 4000 ? 'live' : 'connecting'}</span>
        <button class="btn btn-sm ${st.outputEnabled ? 'btn-warn' : ''}" data-act="output">${st.outputEnabled ? 'Output control ON: turn off' : 'Enable output control…'}</button>
        <button class="btn btn-sm" data-act="ifaces">Interfaces (I²C, SPI, UART, 1-Wire)…</button>
      </div>
      ${st.outputEnabled ? '<div class="pr-note" style="color:var(--warning)">Output control is on: tap a free GPIO to drive it. Pins used by the kernel or a peripheral are refused.</div>' : '<div class="pr-note">Read-only view. Output control is off.</div>'}
      ${st.rebootRequired ? '<div class="pr-note" style="color:var(--warning)">config.txt changed: reboot required. <button class="btn btn-sm btn-warn" data-act="reboot">Reboot now…</button></div>' : ''}
      ${err ? `<div class="pr-note" style="color:var(--danger)">${esc(err)}</div>` : ''}
      ${!pins.length ? '<div class="pr-note">Waiting for the first reading…</div>' : view === 'header' ? headerHtml(header) : tableHtml(bank)}
      ${soc.length ? `<details id="gp-soc" ${socOpen ? 'open' : ''} style="margin-top:10px"><summary class="pr-note" style="cursor:pointer">SoC-internal GPIO 28–${soc[soc.length - 1].gpio} (Ethernet, SD, Wi-Fi/BT, LEDs; read-only)</summary>${tableHtml(soc)}</details>` : ''}`;
  }

  // ---------- dialogs ----------

  function pinDlg(g) {
    const p = byGpio().get(g);
    if (!p) return;
    showModal(`<h3>GPIO${g} · header pin ${p.header}</h3>
      <p>Now: ${esc(funcText(p))}, ${p.level ? 'high' : 'low'}${esc(pullText(p))}. This drives the physical pin: make sure nothing connected minds.</p>
      <div class="pr-row"><span class="pr-note" style="min-width:60px">Mode</span><button class="btn btn-sm" data-m="high">Output high</button><button class="btn btn-sm" data-m="low">Output low</button><button class="btn btn-sm" data-m="input">Input</button></div>
      <div class="pr-row"><span class="pr-note" style="min-width:60px">Pull</span><button class="btn btn-sm" data-p="up">Up</button><button class="btn btn-sm" data-p="down">Down</button><button class="btn btn-sm" data-p="none">None</button></div>
      <div class="pr-note" id="gp-res"></div>
      <div class="modal-btns"><button class="btn btn-primary" id="mk">Done</button></div>`);
    Q('#mk').onclick = hideModal;
    Q('#modal').querySelectorAll('[data-m],[data-p]').forEach(b => b.onclick = async () => {
      try {
        const r = await papi('POST', `/api/gpio/${g}`, b.dataset.m ? { mode: b.dataset.m } : { pull: b.dataset.p });
        Q('#gp-res').textContent = `Now ${funcText(r)}, ${r.level ? 'high' : 'low'}${pullText(r)}`;
      } catch (e) { Q('#gp-res').textContent = e.message; }
    });
  }

  async function outputToggle() {
    if (st.outputEnabled) { await papi('POST', '/api/gpio/output-control', { enabled: false }); return; }
    if (!await confirm2('Enable GPIO output control', 'This lets the dashboard, the REST API and MCP agents drive header pins (high/low/input/pulls). Pins used by the kernel or a peripheral stay refused.', 'Setting is saved; turn it off here any time.')) return;
    await papi('POST', '/api/gpio/output-control', { enabled: true, confirm: true });
  }

  function diffHtml(d) {
    return d.split('\n').map(l => `<span class="${l.startsWith('+') && !l.startsWith('+++') ? 'a' : l.startsWith('-') && !l.startsWith('---') ? 'd' : l.startsWith('@@') ? 'h' : ''}">${esc(l)}</span>`).join('\n');
  }

  // The interfaces dialog, whole or focused on one group: the I²C, Serial and 1-Wire panels
  // open it on their own group (window.gpioInterfaces({ focus: 'i2c' })). A focused dialog
  // only sends its own group to /stage, so it can't change the others.
  const SPEEDS = [10000, 50000, 100000, 400000, 1000000];
  const GROUPS = { i2c: 'I²C buses', spi: 'SPI', uart: 'UARTs', w1: '1-Wire' };
  const hdrPin = g => (st?.header || S?.peripherals?.state?.gpio?.header || []).find(h => h.gpio === g)?.pin;
  const gp = (...gs) => `GPIO${gs.join('/')}${gs.every(hdrPin) ? ` (pin${gs.length > 1 ? 's' : ''} ${gs.map(hdrPin).join('/')})` : ''}`;
  const kHz = hz => hz ? `${hz / 1000} kHz` : '';
  const speedSel = (id, cur) => `<label>Speed <select id="${id}">${SPEEDS.map(v => `<option value="${v}" ${(cur || 100000) === v ? 'selected' : ''}>${v / 1000} kHz${v === 100000 ? ' (default)' : ''}</option>`).join('')}${cur && !SPEEDS.includes(cur) ? `<option value="${cur}" selected>${cur / 1000} kHz</option>` : ''}</select></label>`;

  async function ifaceDlg(focus) {
    let info;
    try { info = await papi('GET', '/api/gpio/interfaces'); } catch { return; }
    const c = info.configured, L = info.live, cat = info.catalog;
    const show = g => !focus || focus === g;
    const row = (id, name, on, liveTxt, extra = '') => `<div class="pr-card"><h4><label style="display:flex;gap:6px;align-items:center"><input type="checkbox" id="gi-${id}" ${on ? 'checked' : ''}> ${name}</label></h4><div class="pr-note">Now: ${liveTxt}</div>${extra}</div>`;
    let h = '';
    if (show('i2c')) {
      h += row('i2c', `I²C1 on ${gp(2, 3)}`, c.i2c.enabled, L.i2c.enabled ? `on, ${L.i2c.dev || 'no /dev/i2c-1'}${L.i2c.hz ? ', ' + kHz(L.i2c.hz) : ''}` : 'off', `<div class="pr-row">${speedSel('gi-baud', c.i2c.baudrate)}</div>`);
      if (cat) {
        for (const [n, x] of Object.entries(cat.extraI2c)) {
          const cf = c.extraI2c[n], lv = L.extraI2c?.[n];
          h += row(n, `${n.replace('i2c', 'I²C')} <small>/dev/i2c-${n.slice(3)}</small>`, cf.enabled, lv?.enabled ? `on, ${lv.dev || 'no device node'}${lv.hz ? ', ' + kHz(lv.hz) : ''}` : 'off',
            `<div class="pr-row"><label>Pins <select id="gi-${n}-pins">${Object.entries(x.choices).map(([k, p]) => `<option value="${k}" ${cf.pins === k ? 'selected' : ''}>${gp(...p)}${k === x.def ? ' (default)' : ''}</option>`).join('')}</select></label>${speedSel(`gi-${n}-baud`, cf.baudrate)}</div>`);
        }
        const g = c.i2cGpio;
        h += row('i2cgpio', 'Software I²C (i2c-gpio, any two pins)', g.enabled, L.i2cGpio?.nodes?.length ? `on (${esc(L.i2cGpio.nodes.join(', '))})` : 'off',
          `<div class="pr-row"><label>SDA GPIO <input id="gi-ig-sda" type="number" min="0" max="27" value="${g.sda}" style="width:56px"></label><label>SCL GPIO <input id="gi-ig-scl" type="number" min="0" max="27" value="${g.scl}" style="width:56px"></label><label>Bus <input id="gi-ig-bus" type="number" min="1" max="255" placeholder="auto" value="${g.bus ?? ''}" style="width:60px"></label><label>Delay µs <input id="gi-ig-delay" type="number" min="1" max="100" placeholder="2" value="${g.delayUs ?? ''}" style="width:56px"></label></div><div class="pr-note">Bit-banged by the CPU: slower (2 µs ≈ 100 kHz) but works on any two free pins.</div>`);
      }
    }
    if (show('spi')) h += row('spi', `SPI0 on ${gp(7, 8, 9, 10, 11)}`, c.spi.enabled, L.spi.enabled ? `on, ${L.spi.devices.join(', ') || 'no spidev nodes'}` : 'off');
    if (show('uart')) {
      h += row('uart', `UART serial0 on ${gp(14, 15)}`, c.uart.enabled, `${L.uart.serial0 ? 'serial0 → ' + L.uart.serial0 : 'no serial0'}${L.uart.consoleOnSerial ? `; <b>serial console on ${esc(L.uart.consoleOnSerial)}</b> (cmdline.txt; not changed here)` : '; no serial console'}${L.uart.note ? '. ' + esc(L.uart.note) : ''}`);
      if (cat) for (const [n, p] of Object.entries(cat.extraUart)) {
        const cf = c.extraUart[n], lv = L.extraUart?.[n];
        h += row(n, `${n.toUpperCase()} on ${gp(p.tx, p.rx)} <small>TX/RX</small>`, cf.enabled, lv?.enabled ? `on${lv.tty ? `, /dev/${esc(lv.tty)}` : ', no tty yet'}` : 'off',
          `<div class="pr-row"><label><input type="checkbox" id="gi-${n}-cts" ${cf.ctsrts ? 'checked' : ''}> CTS/RTS on ${gp(p.cts, p.rts)}</label></div>`);
      }
    }
    if (show('w1')) h += row('w1', '1-Wire (w1-gpio)', c.onewire.enabled, L.onewire.nodes.length ? L.onewire.nodes.map(n => `GPIO${n.gpio}`).join(', ') + `, ${L.onewire.masters} master(s)` : 'off',
      `<div class="pr-row"><label>Data pin GPIO <input id="gi-w1pin" type="number" min="2" max="27" value="${c.onewire.pin ?? 4}" style="width:60px"></label><span class="pr-note">needs a 4.7 kΩ pull-up to 3V3</span></div>`);
    const used = info.pins.map(u => `GPIO${u.gpio} ${esc(u.owner)} ${esc(u.role)}`).join(' · ');
    showModal(`<h3>${focus ? `Interfaces: ${GROUPS[focus]}` : 'Interfaces'}</h3>
      <p class="pr-note">Edits ${esc(info.configPath)}${info.testCopy ? ' (test copy)' : ''}: only lines in the [all] context, new lines under an [all] block. You review the diff before anything is written. Changes need a reboot.${focus ? ' <a href="#" id="gi-all">All interfaces…</a>' : ''}</p>
      ${!cat && (show('i2c') || show('uart')) ? `<p class="pr-note">Extra I²C buses and UARTs are offered on Pi 4 / 400 / CM4 (BCM2711) only; this is ${esc(L.board?.model || 'another board')}.</p>` : ''}
      ${h}
      <p class="pr-note">Pins in use: ${used || 'none'}</p>
      ${info.conflicts.length ? `<p class="pr-note" style="color:var(--danger)">Pin conflicts in config.txt now: ${info.conflicts.map(x => `GPIO${x.gpio} (${esc(x.users.join(', '))})`).join('; ')}</p>` : ''}
      ${c.conditional.length ? `<p class="pr-note" style="color:var(--warning)">Also set in conditional sections: ${c.conditional.map(x => `[${esc(x.section)}] ${esc(x.line)}`).join('; ')}</p>` : ''}
      <div id="gi-out"></div>
      <div class="modal-btns"><button class="btn" id="mc">Close</button><button class="btn btn-primary" id="mk">Review changes</button></div>`, 'wide');
    Q('#mc').onclick = hideModal;
    if (Q('#gi-all')) Q('#gi-all').onclick = e => { e.preventDefault(); ifaceDlg(); };
    Q('#mk').onclick = async () => {
      const on = id => Q(`#gi-${id}`).checked, num = id => Q(`#gi-${id}`).value === '' ? null : Number(Q(`#gi-${id}`).value);
      const req = {};
      if (show('i2c')) {
        req.i2c = { enabled: on('i2c'), baudrate: num('baud') };
        if (cat) {
          req.extraI2c = Object.fromEntries(Object.keys(cat.extraI2c).map(n => [n, { enabled: on(n), pins: Q(`#gi-${n}-pins`).value, baudrate: num(`${n}-baud`) }]));
          req.i2cGpio = { enabled: on('i2cgpio'), ...(on('i2cgpio') ? { sda: num('ig-sda'), scl: num('ig-scl'), bus: num('ig-bus'), delayUs: num('ig-delay') } : {}) };
        }
      }
      if (show('spi')) req.spi = { enabled: on('spi') };
      if (show('uart')) {
        req.uart = { enabled: on('uart') };
        if (cat) req.extraUart = Object.fromEntries(Object.keys(cat.extraUart).map(n => [n, { enabled: on(n), ctsrts: on(`${n}-cts`) }]));
      }
      if (show('w1')) req.onewire = { enabled: on('w1'), ...(on('w1') ? { pin: num('w1pin') } : {}) };
      const out = Q('#gi-out');
      let r;
      try { r = await papi('POST', '/api/gpio/interfaces/stage', req); } catch (e) { out.innerHTML = `<p class="pr-note" style="color:var(--danger)">${esc(e.message)}</p>`; return; }
      if (!r.diff) { out.innerHTML = `<p class="pr-note">${esc(r.message)}</p>${r.warnings.map(w => `<div class="pr-note" style="color:var(--warning)">${esc(w)}</div>`).join('')}`; return; }
      out.innerHTML = `<div class="pr-card"><h4>Proposed change<small>${esc(r.summary.join(', '))}</small></h4>
        <div class="pr-log gp-diff">${diffHtml(r.diff)}</div>
        ${r.warnings.map(w => `<div class="pr-note" style="color:var(--warning)">${esc(w)}</div>`).join('')}
        <label class="pr-note" style="display:flex;gap:6px;align-items:center;margin-top:6px"><input type="checkbox" id="gi-ok"> Write this to ${esc(r.configPath)} (a timestamped backup is made first)</label>
        <div class="pr-row"><button class="btn btn-warn" id="gi-apply" disabled>Apply</button></div></div>`;
      Q('#gi-ok').onchange = () => Q('#gi-apply').disabled = !Q('#gi-ok').checked;
      Q('#gi-apply').onclick = async () => {
        try {
          const a = await papi('POST', '/api/gpio/interfaces/apply', { token: r.token, confirm: true });
          out.innerHTML = `<div class="pr-card"><h4>Saved</h4><div class="pr-note">Backup: <span class="pr-endpoint">${esc(a.backup)}</span></div><div class="pr-note" style="color:var(--warning)">Reboot required for ${esc(a.summary.join(', '))}.</div><div class="pr-row"><button class="btn btn-warn" id="gi-reboot">Reboot now…</button></div></div>`;
          Q('#mk').style.display = 'none';
          Q('#gi-reboot').onclick = rebootDlg;
        } catch {}
      };
    };
  }

  function rebootDlg() {
    showModal(`<h3 style="color:var(--warning)">Reboot DeskyPi</h3><p>Everything served from this Pi (drives, bridges, this dashboard) goes away for about a minute.</p>
      <div class="confirm-box"><label>Type REBOOT to confirm:</label><input type="text" id="gi-rb" autocomplete="off"></div>
      <div class="modal-btns"><button class="btn" id="mc">Cancel</button><button class="btn btn-danger" id="mk" disabled>Reboot</button></div>`);
    Q('#gi-rb').oninput = () => Q('#mk').disabled = Q('#gi-rb').value !== 'REBOOT';
    Q('#mc').onclick = hideModal;
    Q('#mk').onclick = async () => { try { await papi('POST', '/api/gpio/reboot', { confirm: 'REBOOT' }); hideModal(); toast('Rebooting…', 'info'); } catch {} };
  }

  // For the other panels. The routes live in this driver, so no button while it's turned off.
  window.gpioInterfaces = o => ifaceDlg(o?.focus).catch(() => {});
  window.gpioIfaceBtn = (focus, label) => S?.peripherals?.list?.some(d => d.id === 'gpio' && !d.disabled) ? `<button class="btn btn-sm" onclick="gpioInterfaces({focus:'${focus}'})">${label}</button>` : '';

  async function onClick(e) {
    const t = e.target.closest('[data-act]');
    if (!t || !root?.contains(t)) return;
    try {
      const a = t.dataset.act;
      if (a === 'view') { view = t.dataset.v; try { localStorage.setItem('gpioView', view); } catch {} render(); }
      else if (a === 'pin') pinDlg(Number(t.dataset.g));
      else if (a === 'output') await outputToggle();
      else if (a === 'ifaces') await ifaceDlg();
      else if (a === 'reboot') rebootDlg();
    } catch {}
  }

  periphRegister('gpio', {
    mount(el, s) {
      root = el; st = s; ensureCss();
      el.addEventListener('click', onClick);
      sock = pws('/ws/gpio', { onmessage: m => { if (m?.type === 'state') { pins = m.pins; err = m.error || ''; live = Date.now(); if (st) st.outputEnabled = m.outputEnabled; render(); } } });
      render();
    },
    update(s) { st = s; render(); },
    unmount() { sock?.close(); sock = null; root = null; pins = []; live = 0; },
  });
})();
