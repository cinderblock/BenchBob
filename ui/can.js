// CAN section: one card per SocketCAN interface (state, bitrate, error counters, rates,
// actions), an opt-in live monitor per interface (per-ID table, frame log, send/periodic,
// decoded errors), SLCAN/vcan tools, recordings, and the socketcand/WebSocket endpoints with
// client snippets.
//
// Traffic never comes through the 2 s status push: an open monitor holds a WebSocket with
// batch=1&stats=1, so the server sends frames in 50 ms batches and an aggregated ID table at
// 4 Hz. Rendering is coalesced into one requestAnimationFrame, so thousands of frames/s cost
// one DOM update per frame-time at most.
(() => {
  let root = null;
  let S0 = null; // last status for this driver
  const cards = {}; // iface -> { el, mon }
  const LS = 'canMonitor';
  let monWanted = {};
  try { monWanted = JSON.parse(localStorage.getItem(LS) || '{}'); } catch {}
  const saveMon = () => { try { localStorage.setItem(LS, JSON.stringify(monWanted)); } catch {} };
  const enc = encodeURIComponent;
  const fmtBr = b => !b ? '?' : b >= 1e6 ? +(b / 1e6).toFixed(3) + 'M' : b >= 1e3 ? +(b / 1e3).toFixed(3) + 'k' : String(b);
  const fmtId = (id, ext) => id.toString(16).toUpperCase().padStart(ext ? 8 : 3, '0');
  const fmtSize = n => n < 1024 ? n + ' B' : n < 1048576 ? (n / 1024).toFixed(1) + ' KB' : (n / 1048576).toFixed(1) + ' MB';
  const stPill = st => {
    if (!st) return '';
    const cls = st === 'ERROR-ACTIVE' ? 'on' : st === 'ERROR-WARNING' ? 'warn' : st === 'STOPPED' ? '' : 'bad';
    return `<span class="pr-pill ${cls}">${esc(st.toLowerCase())}</span>`;
  };
  const LOG_MAX = 300;

  // ---------- interface cards ----------

  function headHtml(i) {
    const busy = i.busy ? `<span class="pr-pill warn">${esc(i.busy)}</span>` : '';
    const timing = i.kind === 'can'
      ? (i.bitrate ? `${fmtBr(i.bitrate)}${i.samplePoint ? ` @ ${(i.samplePoint * 100).toFixed(1)}%` : ''}${i.fdOn && i.dbitrate ? ` · FD data ${fmtBr(i.dbitrate)}${i.dsamplePoint ? ` @ ${(i.dsamplePoint * 100).toFixed(1)}%` : ''}` : ''}` : 'bitrate not set')
      : (i.fdOn ? 'virtual, CAN FD MTU' : 'virtual, classic MTU');
    const flags = i.ctrl.filter(f => f !== 'FD').map(f => `<span class="pr-pill${f === 'LISTEN-ONLY' ? ' warn' : ''}">${esc(f.toLowerCase())}</span>`).join(' ');
    const r = i.rates || {};
    const rates = i.up ? `${r.fps || 0} fps${r.txps ? ` (${r.txps} tx)` : ''}${r.load != null ? ` · load ${r.load}%` : ''}${r.errps ? ` · <span style="color:var(--danger)">${r.errps} err/s</span>` : ''}` : '';
    const berr = i.berr ? `TEC ${i.berr.tx} · REC ${i.berr.rx}` : '';
    const xs = i.xstats ? ['bus_error', 'error_warning', 'error_passive', 'bus_off', 'restarts', 'arbitration_lost'].filter(k => i.xstats[k]).map(k => `${k.replace('_', ' ')} ${i.xstats[k]}`).join(' · ') : '';
    const dev = [i.label, i.driver && i.driver !== i.kind ? i.driver : '', i.usb?.serial ? 'S/N ' + i.usb.serial : '', i.slcanTty ? 'on ' + i.slcanTty : '', i.clock ? `clock ${fmtBr(i.clock)}Hz` : '', i.fdCapable && i.kind === 'can' ? 'FD capable' : ''].filter(Boolean).map(esc).join(' · ');
    const ab = i.autobitrate
      ? `<div class="pr-note">Auto-bitrate: trying ${fmtBr(i.autobitrate.bitrate)} (${i.autobitrate.step}/${i.autobitrate.of}), listen-only</div>`
      : i.lastAutobitrate ? `<div class="pr-note">Last auto-bitrate (${fmtAgo(i.lastAutobitrate.at)}): <b>${esc(i.lastAutobitrate.text)}</b>${i.lastAutobitrate.applied ? ' Applied.' : ''}${i.lastAutobitrate.note ? ' ' + esc(i.lastAutobitrate.note) : ''}</div>` : '';
    const rec = i.recording ? `<div class="pr-note"><span class="pr-pill bad">● REC</span> ${esc(i.recording.file)}: ${i.recording.frames} frames, ${fmtSize(i.recording.bytes)}</div>` : '';
    const reader = i.up && i.reader !== 'open' ? `<div class="pr-note" style="color:var(--danger)">Reader: ${esc(i.reader)}</div>` : '';
    const n = esc(i.name), q = esc(JSON.stringify(i.name));
    const dis = i.busy ? 'disabled' : '';
    const acts = [
      `<button class="btn btn-sm" ${dis} onclick='canUp(${q}, ${!i.up})'>${i.up ? 'Down' : 'Up'}</button>`,
      `<button class="btn btn-sm" ${dis} onclick='canConfig(${q})'>Configure…</button>`,
      i.kind === 'can' || i.kind === 'vcan' ? `<button class="btn btn-sm" ${dis} onclick='canAuto(${q})'>Auto-bitrate</button>` : '',
      i.state === 'BUS-OFF' ? `<button class="btn btn-sm btn-warn" onclick='canRestart(${q})'>Restart controller</button>` : '',
      i.up ? `<button class="btn btn-sm${i.recording ? ' btn-danger' : ''}" onclick='canRecord(${q}, ${!i.recording})'>${i.recording ? 'Stop recording' : 'Record'}</button>` : '',
      `<button class="btn btn-sm${monWanted[i.name] ? ' btn-primary' : ''}" onclick='canMonitor(${q})'>${monWanted[i.name] ? 'Hide monitor' : 'Monitor'}</button>`,
      i.kind === 'vcan' ? `<button class="btn btn-sm btn-danger" onclick='canRemoveVcan(${q})'>Remove vcan</button>` : '',
      i.slcanTty ? `<button class="btn btn-sm btn-danger" onclick='canDetach(${q})'>Detach SLCAN</button>` : '',
    ].join('');
    return `<h4>${n} <span class="pr-pill ${i.up ? 'on' : ''}">${i.up ? 'up' : 'down'}</span> ${i.up ? stPill(i.state) : ''} ${i.fdOn ? '<span class="pr-pill">FD</span>' : ''} ${flags} ${busy}</h4>
      <div class="pr-note">${dev}</div>
      <div style="font-size:.8rem;margin-top:4px">${esc(timing)}${rates ? ' · ' + rates : ''}${berr ? ' · ' + berr : ''}</div>
      ${xs ? `<div class="pr-note">${esc(xs)}</div>` : ''}${i.bringUpOnPlug ? '<div class="pr-note">Brought up automatically when plugged in.</div>' : ''}
      ${ab}${rec}${reader}
      <div class="pr-row">${acts}</div>`;
  }

  function renderIfaces(st) {
    const host = root.querySelector('.can-ifaces');
    const names = new Set(st.ifaces.map(i => i.name));
    for (const n of Object.keys(cards)) if (!names.has(n)) { closeMon(n); cards[n].el.remove(); delete cards[n]; }
    if (!st.ifaces.length) host.innerHTML = '<div class="pr-note">No CAN interfaces. Plug in a candleLight/CANable/PEAK adapter, attach an SLCAN adapter, or create a virtual one below.</div>';
    else host.querySelector(':scope > .pr-note')?.remove();
    for (const i of st.ifaces) {
      let k = cards[i.name];
      if (!k) {
        const el = document.createElement('div');
        el.className = 'pr-card';
        el.innerHTML = '<div class="can-head"></div><div class="can-mon"></div>';
        host.appendChild(el);
        k = cards[i.name] = { el, mon: null, info: i };
      }
      k.info = i;
      const html = headHtml(i);
      if (k.headHtml !== html) { k.el.querySelector('.can-head').innerHTML = html; k.headHtml = html; }
      if (monWanted[i.name] && !k.mon) openMon(i.name);
      if (!monWanted[i.name] && k.mon) closeMon(i.name);
    }
  }

  // ---------- live monitor ----------

  function openMon(name) {
    const k = cards[name]; if (!k) return;
    const box = k.el.querySelector('.can-mon');
    const q = esc(JSON.stringify(name));
    box.innerHTML = `
      <div class="pr-note can-mstat" style="margin-top:10px">Connecting…</div>
      <div style="overflow-x:auto;max-height:340px;overflow-y:auto;margin-top:6px"><table class="pr-tbl"><thead><tr><th>ID</th><th>Len</th><th>Count</th><th>Period</th><th>Last data</th><th>Seen</th></tr></thead><tbody class="can-ids"></tbody></table></div>
      <div class="can-errs"></div>
      <div class="pr-row" style="margin-top:12px">
        <label>Log filter <input class="can-filter" placeholder="7E8, 100:700, x18DAF100:1FFFFF00" style="width:220px"></label>
        <button class="btn btn-sm can-pause" onclick='canPause(${q})'>Pause</button>
        <button class="btn btn-sm" onclick='canClear(${q})'>Clear</button>
      </div>
      <div class="pr-note">Filter: hex IDs, optional :mask, x prefix for extended. Applied on the server, so the log only carries what matches; the ID table always shows everything.</div>
      <div class="pr-log can-log" style="height:220px"></div>
      <div class="pr-row" style="margin-top:12px">
        <label>ID <input class="can-sid" placeholder="123" style="width:90px"></label>
        <label>Data <input class="can-sdata" placeholder="DE AD BE EF" style="width:170px"></label>
        <label><input type="checkbox" class="can-sext"> ext</label>
        <label><input type="checkbox" class="can-srtr"> RTR</label>
        <label><input type="checkbox" class="can-sfd"> FD</label>
        <label><input type="checkbox" class="can-sbrs"> BRS</label>
        <button class="btn btn-sm btn-primary" onclick='canSend(${q})'>Send</button>
        <label>every <input class="can-sper" type="number" min="5" value="100" style="width:70px"> ms</label>
        <button class="btn btn-sm" onclick='canPeriodic(${q})'>Start periodic</button>
      </div>
      <div class="pr-note">ID and data are hex. You can also type cansend syntax in ID, e.g. <span class="pr-endpoint">123#DEADBEEF</span> or <span class="pr-endpoint">123##1AABBCC</span> (FD+BRS). Frames you send appear in the log as <b>tx</b> once the controller confirms them; with nothing else on the bus to ACK, they never do.</div>
      <div class="can-jobs"></div>`;
    const m = k.mon = { name, box, log: [], paused: false, stats: null, dirty: { log: false, stats: false }, raf: 0, ws: null };
    const inp = box.querySelector('.can-filter');
    let t = 0;
    inp.oninput = () => { clearTimeout(t); t = setTimeout(() => m.ws?.send({ filter: inp.value.trim() || null }), 400); };
    box.querySelector('.can-sid').onkeydown = e => { if (e.key === 'Enter') canSend(name); };
    box.querySelector('.can-sdata').onkeydown = e => { if (e.key === 'Enter') canSend(name); };
    m.ws = pws(`/ws/can/${enc(name)}?batch=1&stats=1`, {
      onopen: () => { if (inp.value.trim()) m.ws.send({ filter: inp.value.trim() }); if (m.paused) m.ws.send({ frames: false }); },
      onmessage: msg => {
        if (msg.type === 'hello') { if (!m.paused) { m.log = []; addFrames(m, msg.recent || []); } }
        else if (msg.type === 'frames') { if (!m.paused) addFrames(m, msg.frames, msg.dropped); }
        else if (msg.type === 'stats') { m.stats = msg; m.dirty.stats = true; schedule(m); }
        else if (msg.type === 'error') toast(msg.message, 'error');
      },
      onclose: () => { const s = box.querySelector('.can-mstat'); if (s) s.textContent = 'Disconnected, retrying…'; },
    });
  }

  function closeMon(name) {
    const k = cards[name]; if (!k?.mon) return;
    k.mon.ws?.close();
    cancelAnimationFrame(k.mon.raf);
    k.mon = null;
    k.el.querySelector('.can-mon').innerHTML = '';
  }

  function fmtTs(ts) {
    const d = new Date(ts * 1000);
    return d.toTimeString().slice(0, 8) + '.' + String(d.getMilliseconds()).padStart(3, '0');
  }

  function frameLine(f) {
    if (f.err) return `${fmtTs(f.ts)} ERR ${f.text || ''} [${f.data}]`;
    const data = f.rtr ? `remote request, len ${f.len}` : f.data.replace(/(..)(?=.)/g, '$1 ');
    return `${fmtTs(f.ts)} ${f.dir} ${fmtId(f.id, f.ext).padStart(8)} ${f.fd ? (f.brs ? 'FD+BRS' : 'FD    ') : '      '} [${String(f.len).padStart(2)}] ${data}`;
  }

  function addFrames(m, frames, dropped) {
    for (const f of frames) m.log.push(frameLine(f));
    if (dropped) m.log.push(`… ${dropped} frames not shown (browser or link too slow)`);
    if (m.log.length > LOG_MAX * 2) m.log.splice(0, m.log.length - LOG_MAX);
    m.dirty.log = true;
    schedule(m);
  }

  function schedule(m) {
    if (!m.raf) m.raf = requestAnimationFrame(() => { m.raf = 0; draw(m); });
  }

  function dataHtml(hex, hot) {
    let s = '';
    for (let b = 0; b * 2 < hex.length; b++) {
      const byte = hex.substr(b * 2, 2);
      s += (b ? ' ' : '') + (hot & (1 << b) ? `<b style="color:var(--warning)">${byte}</b>` : byte);
    }
    return s;
  }

  function draw(m) {
    const box = m.box;
    if (!box.isConnected) return;
    if (m.dirty.log) {
      m.dirty.log = false;
      const el = box.querySelector('.can-log');
      const atBottom = el.scrollTop + el.clientHeight >= el.scrollHeight - 30;
      el.textContent = m.log.slice(-LOG_MAX).join('\n');
      if (atBottom) el.scrollTop = el.scrollHeight;
    }
    if (m.dirty.stats && m.stats) {
      m.dirty.stats = false;
      const s = m.stats, r = s.rates || {};
      box.querySelector('.can-mstat').innerHTML = `${s.up ? '' : '<b style="color:var(--danger)">Interface is down.</b> '}${r.fps || 0} frames/s${r.load != null ? ` · bus load ${r.load}%` : ''} · ${s.idCount} IDs · total rx ${s.totals.rx}, tx ${s.totals.tx}, errors ${s.totals.err}${s.drops ? ` · <span style="color:var(--danger)">${s.drops} dropped by the kernel (receive queue overflow)</span>` : ''}${m.paused ? ' · <b>log paused</b>' : ''}`;
      const now = Date.now();
      box.querySelector('.can-ids').innerHTML = (s.ids || []).map(r =>
        `<tr><td style="font-family:monospace">${fmtId(r.id, r.ext)}${r.ext ? ' <span class="pr-pill">ext</span>' : ''}${r.fd ? ' <span class="pr-pill">FD</span>' : ''}${r.dir === 'tx' ? ' <span class="pr-pill on">tx</span>' : ''}</td><td>${r.len}</td><td>${r.count}</td><td>${r.count > 1 ? (r.period >= 1000 ? (r.period / 1000).toFixed(2) + ' s' : r.period.toFixed(1) + ' ms') : '—'}</td><td style="font-family:monospace;white-space:nowrap">${r.rtr ? 'RTR' : dataHtml(r.data, r.hot)}</td><td style="white-space:nowrap">${now - r.last < 1500 ? 'now' : fmtAgo(r.last)}</td></tr>`).join('')
        || '<tr><td colspan="6" class="pr-note">No frames yet</td></tr>';
      const errs = (s.errors || []).slice(-5).reverse();
      box.querySelector('.can-errs').innerHTML = errs.length ? `<div class="pr-note" style="margin-top:8px"><b>Recent error frames</b></div>${errs.map(e => `<div class="pr-note">${new Date(e.ts).toLocaleTimeString()}: ${esc(e.text)}${e.tec != null ? ` (TEC ${e.tec}, REC ${e.rec})` : ''}</div>`).join('')}` : '';
      const q = esc(JSON.stringify(m.name));
      box.querySelector('.can-jobs').innerHTML = (s.jobs || []).length ? `<div class="pr-note" style="margin-top:8px"><b>Periodic jobs</b></div>${s.jobs.map(j => `<div class="pr-row"><span class="pr-endpoint">${esc(j.frame)}</span><span class="pr-note">every ${j.periodMs} ms · sent ${j.sent}${j.errors ? ` · ${j.errors} errors${j.lastErr ? ': ' + esc(j.lastErr) : ''}` : ''}</span><button class="btn btn-sm" onclick='canStopJob(${q}, ${j.id})'>Stop</button></div>`).join('')}` : '';
    }
  }

  // ---------- actions ----------

  const frameFromForm = name => {
    const box = cards[name]?.mon?.box; if (!box) return null;
    const v = c => box.querySelector(c);
    const id = v('.can-sid').value.trim();
    if (!id) { toast('Enter an ID (hex)', 'error'); return null; }
    if (id.includes('#')) return { cansend: id };
    return { id, data: v('.can-sdata').value.trim(), ext: v('.can-sext').checked || undefined, rtr: v('.can-srtr').checked || undefined, fd: v('.can-sfd').checked || undefined, brs: v('.can-sbrs').checked || undefined };
  };
  window.canSend = async name => { const f = frameFromForm(name); if (f) try { await papi('POST', `/api/can/${enc(name)}/send`, f); } catch {} };
  window.canPeriodic = async name => {
    const f = frameFromForm(name); if (!f) return;
    const periodMs = Number(cards[name].mon.box.querySelector('.can-sper').value);
    try { await papi('POST', `/api/can/${enc(name)}/periodic`, { ...f, periodMs }); toast('Periodic job started', 'success'); } catch {}
  };
  window.canStopJob = async (name, id) => { try { await papi('DELETE', `/api/can/${enc(name)}/periodic/${id}`); } catch {} };
  window.canPause = name => {
    const m = cards[name]?.mon; if (!m) return;
    m.paused = !m.paused;
    m.ws.send({ frames: !m.paused }); // stop the server sending frames we'd throw away
    m.box.querySelector('.can-pause').textContent = m.paused ? 'Resume' : 'Pause';
    m.dirty.stats = true; schedule(m);
  };
  window.canClear = async name => {
    const m = cards[name]?.mon;
    if (m) { m.log = []; m.dirty.log = true; schedule(m); }
    try { await papi('POST', `/api/can/${enc(name)}/clear`); } catch {}
  };
  window.canMonitor = name => { monWanted[name] = !monWanted[name]; if (!monWanted[name]) delete monWanted[name]; saveMon(); if (S0) renderIfaces(S0); };
  window.canUp = async (name, up) => { try { await papi('POST', `/api/can/${enc(name)}/config`, { up }); } catch {} };
  window.canRestart = async name => { try { await papi('POST', `/api/can/${enc(name)}/restart`); } catch {} };
  window.canRecord = async (name, on) => {
    try { const r = await papi('POST', `/api/can/${enc(name)}/record`, { on }); toast(on ? `Recording to ${r.file}` : `Saved ${r.file} (${r.frames} frames)`, 'success'); } catch {}
  };
  window.canRemoveVcan = async name => { try { await papi('DELETE', `/api/can/vcan/${enc(name)}`); } catch {} };
  window.canCreateVcan = async () => { try { const r = await papi('POST', '/api/can/vcan', { fd: true }); toast(`Created ${r.name}`, 'success'); } catch {} };
  window.canDetach = async name => { try { await papi('DELETE', `/api/can/slcan/${enc(name)}`); } catch {} };
  window.canAttach = async (tty, selId) => {
    const bitrate = Number(document.getElementById(selId)?.value) || 500000;
    try { const r = await papi('POST', '/api/can/slcan', { tty, bitrate }); toast(`Attached as ${r.name}`, 'success'); } catch {}
  };
  window.canAttachManual = () => { const t = root.querySelector('.can-mtty').value.trim(); if (t) canAttach(t, 'can-mbr'); };
  window.canDelRec = async file => { try { await papi('DELETE', `/api/can/recordings/${enc(file)}`); } catch {} };

  window.canAuto = name => {
    const i = cards[name]?.info; if (!i) return;
    const q = esc(JSON.stringify(name));
    showModal(`<h3>Auto-detect bitrate on ${esc(name)}</h3>
      <p class="pr-note">Tries each standard rate (1M down to 10k) in <b>listen-only</b> mode: the controller never transmits or ACKs, so this is safe on a live bus. At each rate it listens briefly and counts clean frames against error frames and bus errors. It takes about ${(S0?.standardBitrates?.length || 11) * 0.6 | 0} s, and the interface goes down and up once per rate.${i.kind === 'vcan' ? ' <b>vcan has no bit timing</b>: every rate sees the same traffic, so this only exercises the procedure.' : ''}</p>
      <div class="pr-row"><label>Listen per rate <input id="can-ab-ms" type="number" min="100" max="5000" value="400" style="width:80px"> ms</label></div>
      <div class="pr-row"><label><input id="can-ab-apply" type="checkbox"> Apply the detected bitrate (otherwise the old config is restored)</label></div>
      <div class="modal-btns"><button class="btn" onclick="hideModal()">Cancel</button><button class="btn btn-primary" onclick='canAutoGo(${q})'>Start</button></div>`);
  };
  window.canAutoGo = async name => {
    const listenMs = Number(document.getElementById('can-ab-ms').value) || 400;
    const apply = document.getElementById('can-ab-apply').checked;
    hideModal();
    toast(`Auto-bitrate running on ${name}…`, 'info');
    try {
      const r = await papi('POST', `/api/can/${enc(name)}/autobitrate`, { listenMs, apply });
      showModal(`<h3>Auto-bitrate: ${esc(name)}</h3><p><b>${esc(r.text)}</b></p>${r.note ? `<p class="pr-note">${esc(r.note)}</p>` : ''}
        <div style="overflow-x:auto"><table class="pr-tbl"><thead><tr><th>Bitrate</th><th>Frames</th><th>Error frames</th><th>Bus errors</th></tr></thead><tbody>${r.trials.map(t => `<tr${t.bitrate === r.bitrate ? ' style="color:var(--success)"' : ''}><td>${fmtBr(t.bitrate)}</td>${t.skipped ? `<td colspan="3" class="pr-note">skipped: ${esc(t.skipped)}</td>` : `<td>${t.frames}</td><td>${t.errors}</td><td>${t.busErrors}</td>`}</tr>`).join('')}</tbody></table></div>
        <div class="modal-btns"><button class="btn" onclick="hideModal()">Close</button></div>`);
    } catch {}
  };

  window.canConfig = name => {
    const i = cards[name]?.info; if (!i) return;
    const q = esc(JSON.stringify(name));
    const sv = i.saved || {};
    const chk = (id, label, on, note) => `<label style="display:flex;gap:6px;align-items:flex-start;font-size:.82rem;margin:4px 0"><input type="checkbox" id="${id}" ${on ? 'checked' : ''}><span>${label}${note ? `<br><span class="pr-note">${note}</span>` : ''}</span></label>`;
    const sup = f => !i.ctrlSupported || i.ctrlSupported.includes(f);
    const rates = (S0?.standardBitrates || []).map(b => `<option value="${b}" ${b === (i.bitrate || 500000) ? 'selected' : ''}>${fmtBr(b)}</option>`).join('');
    let h = `<h3>Configure ${esc(name)}</h3>`;
    if (i.kind === 'can') {
      h += `<div class="pr-row"><label>Bitrate <select id="can-c-br">${rates}<option value="custom">custom…</option></select></label><label>custom <input id="can-c-brc" type="number" placeholder="bit/s" style="width:100px" value="${i.bitrate && !(S0?.standardBitrates || []).includes(i.bitrate) ? i.bitrate : ''}"></label></div>
        <div class="pr-row"><label>Sample point <input id="can-c-sp" type="number" step="0.1" min="50" max="95" placeholder="auto" value="${i.samplePoint ? (i.samplePoint * 100).toFixed(1) : ''}" style="width:80px"> %</label><span class="pr-note">Empty lets the kernel pick (CiA recommends 87.5%)</span></div>`;
      if (i.fdCapable) h += `${chk('can-c-fd', 'CAN FD', i.fdOn)}<div class="pr-row"><label>Data bitrate <input id="can-c-dbr" type="number" placeholder="2000000" value="${i.dbitrate || ''}" style="width:110px"></label><label>Data sample point <input id="can-c-dsp" type="number" step="0.1" placeholder="auto" value="${i.dsamplePoint ? (i.dsamplePoint * 100).toFixed(1) : ''}" style="width:70px"> %</label></div>`;
      else h += '<p class="pr-note">This controller does not support CAN FD.</p>';
      if (sup('LISTEN-ONLY')) h += chk('can-c-lo', 'Listen-only', i.ctrl.includes('LISTEN-ONLY'), 'Never transmits or ACKs. Safe for sniffing a live bus.');
      if (sup('LOOPBACK')) h += chk('can-c-lb', 'Loopback', i.ctrl.includes('LOOPBACK'), 'Controller internal loopback: test sending without a bus.');
      if (sup('TRIPLE-SAMPLING')) h += chk('can-c-ts', 'Triple sampling', i.ctrl.includes('TRIPLE-SAMPLING'));
      h += `<div class="pr-row"><label>Auto-restart after bus-off <input id="can-c-rms" type="number" min="0" value="${i.restartMs ?? 0}" style="width:80px"> ms</label><span class="pr-note">0 = stay bus-off until restarted by hand</span></div>`;
    } else h += `<p class="pr-note">A virtual interface has no bit timing. FD here only sets the 72-byte MTU.</p>${chk('can-c-fd', 'CAN FD frames', i.fdOn)}`;
    h += chk('can-c-plug', 'Bring up on plug', sv.bringUpOnPlug, `Re-apply this config whenever this adapter appears. Saved per adapter (${esc(i.key)}).`);
    h += `<div class="modal-btns"><button class="btn" onclick="hideModal()">Cancel</button><button class="btn" onclick='canConfigGo(${q}, false)'>Apply, leave down</button><button class="btn btn-primary" onclick='canConfigGo(${q}, true)'>Apply and bring up</button></div>`;
    showModal(h);
  };
  window.canConfigGo = async (name, up) => {
    const g = id => document.getElementById(id);
    const c = { up, bringUpOnPlug: g('can-c-plug').checked };
    if (g('can-c-fd')) c.fd = g('can-c-fd').checked;
    if (g('can-c-br')) {
      c.bitrate = Number(g('can-c-br').value === 'custom' ? g('can-c-brc').value : g('can-c-br').value) || undefined;
      if (g('can-c-sp').value) c.samplePoint = Number(g('can-c-sp').value) / 100;
      if (c.fd) { c.dbitrate = Number(g('can-c-dbr').value) || undefined; if (g('can-c-dsp').value) c.dsamplePoint = Number(g('can-c-dsp').value) / 100; }
      for (const [id, k] of [['can-c-lo', 'listenOnly'], ['can-c-lb', 'loopback'], ['can-c-ts', 'tripleSampling']]) if (g(id)) c[k] = g(id).checked;
      c.restartMs = Number(g('can-c-rms').value) || 0;
    }
    hideModal();
    try { await papi('POST', `/api/can/${enc(name)}/config`, c); toast(`${name} configured`, 'success'); } catch {}
  };

  window.canScApply = async () => {
    const g = c => root.querySelector(c);
    try { await papi('POST', '/api/can/socketcand', { enabled: g('.can-sc-en').checked, port: Number(g('.can-sc-port').value), beacon: g('.can-sc-bc').checked }); toast('socketcand settings saved', 'success'); } catch {}
  };

  // ---------- tools, recordings, endpoints ----------

  function renderTools(st) {
    const el = root.querySelector('.can-tools');
    const brOpts = (st.slcanBitrates || []).map(b => `<option value="${b}" ${b === 500000 ? 'selected' : ''}>${fmtBr(b)}</option>`).join('');
    const cands = st.slcan.candidates.map((c, n) => `<div class="pr-row"><span><b>${esc(c.kind)}</b> <span class="pr-note">${esc(c.tty)}${c.serial ? ' · S/N ' + esc(c.serial) : ''}</span></span>${c.attached
      ? `<span class="pr-pill on">attached as ${esc(c.attached)}</span><button class="btn btn-sm" onclick='canDetach(${esc(JSON.stringify(c.attached))})'>Detach</button>`
      : `<select id="can-slbr-${n}">${brOpts}</select><button class="btn btn-sm btn-primary" onclick='canAttach(${esc(JSON.stringify(c.tty))}, "can-slbr-${n}")'>Attach as SLCAN</button>`}</div>`).join('');
    const sig = JSON.stringify([st.slcan, st.recordings]);
    if (el.dataset.sig === sig) return;
    el.dataset.sig = sig;
    el.innerHTML = `<div class="pr-card"><h4>Adapters and test interfaces</h4>
      ${cands || '<div class="pr-note">No SLCAN serial adapters detected (CANable slcan firmware 16d0:117e, Lawicel CANUSB 0403:ffa8).</div>'}
      <div class="pr-note">Attaching runs <span class="pr-endpoint">slcand</span> on the serial port and creates a SocketCAN interface. While attached, the serial panel can't open that port; detach to use it as a serial port again.</div>
      <details style="margin-top:6px"><summary class="pr-note" style="cursor:pointer">Attach another serial device as SLCAN</summary>
        <div class="pr-row"><input class="can-mtty" placeholder="/dev/ttyACM0" style="width:150px"><select id="can-mbr">${brOpts}</select><button class="btn btn-sm" onclick="canAttachManual()">Attach</button></div></details>
      <div class="pr-row" style="margin-top:8px"><button class="btn btn-sm" onclick="canCreateVcan()">Create virtual CAN (vcan) for testing</button><span class="pr-note">A software bus with no hardware: everything sent on it is received by every listener on it.</span></div></div>
      ${st.recordings.length ? `<div class="pr-card"><h4>Recordings <small>candump -L format; replay with canplayer -I file</small></h4>${st.recordings.map(r => `<div class="pr-row"><a class="btn btn-sm" href="/api/can/recordings/${enc(r.file)}" download>${esc(r.file)}</a><span class="pr-note">${fmtSize(r.size)} · ${fmtAgo(r.mtime)}</span><button class="btn btn-sm btn-danger" onclick='canDelRec(${esc(JSON.stringify(r.file))})'>Delete</button></div>`).join('')}</div>` : ''}`;
  }

  function renderEndpoints(st) {
    const el = root.querySelector('.can-ep');
    const sc = st.socketcand;
    const host = location.hostname;
    const bus = st.ifaces[0]?.name || 'can0';
    const sig = JSON.stringify([sc, bus, host]);
    if (el.dataset.sig === sig) return;
    // Don't rebuild while the user is typing a port.
    if (el.contains(document.activeElement) && document.activeElement.tagName === 'INPUT') return;
    el.dataset.sig = sig;
    const py = `import can\nbus = can.Bus(interface="socketcand", host="${host}", port=${sc.port}, channel="${bus}")\nbus.send(can.Message(arbitration_id=0x123, data=[0xDE, 0xAD], is_extended_id=False))\nfor msg in bus:\n    print(msg)`;
    const wsjs = `const ws = new WebSocket("ws://${location.host}/ws/can/${bus}?filter=7E8");\nws.onmessage = e => console.log(JSON.parse(e.data));\nws.onopen = () => ws.send(JSON.stringify({ send: { id: "7DF", data: "0201000000000000" } }));`;
    el.innerHTML = `<div class="pr-card"><h4>Network access <small>for programs and other machines</small></h4>
      <div class="pr-row"><label><input type="checkbox" class="can-sc-en" ${sc.enabled ? 'checked' : ''}> socketcand server</label><label>port <input class="can-sc-port" type="number" value="${sc.port}" style="width:80px"></label><label><input type="checkbox" class="can-sc-bc" ${sc.beacon ? 'checked' : ''}> discovery beacon (UDP 42000)</label><button class="btn btn-sm" onclick="canScApply()">Apply</button></div>
      <div class="pr-note">${sc.listening ? `Listening on <span class="pr-endpoint">${esc(host)}:${sc.port}</span>` : sc.enabled ? `<span style="color:var(--danger)">Not listening: ${esc(sc.error || 'starting')}</span>` : 'Off'}${sc.beacon && sc.beaconTo ? ` · beacon to ${esc(sc.beaconTo)}` : ''}. Raw and BCM modes of the socketcand protocol, classic CAN frames only (no FD), no bus configuration from the client.</div>
      ${sc.clients.length ? sc.clients.map(c => `<div class="pr-note">Client ${esc(c.peer)} on ${esc(c.iface || '(no bus)')} · ${esc(c.mode)} mode · rx ${c.rx} · tx ${c.tx}${c.dropped ? ` · ${c.dropped} dropped` : ''}</div>`).join('') : '<div class="pr-note">No clients connected.</div>'}
      <details style="margin-top:8px"><summary class="pr-note" style="cursor:pointer">python-can</summary><pre class="pr-log" style="user-select:all">${esc(py)}</pre><div class="pr-note">Needs python-can 4.x. With the beacon on, <span class="pr-endpoint">can.detect_available_configs("socketcand")</span> finds BenchBob by itself.</div></details>
      <details><summary class="pr-note" style="cursor:pointer">SavvyCAN</summary><div class="pr-note" style="margin:6px 0">Connection → Add New Device Connection → <b>socketcand</b>. Enter host <span class="pr-endpoint">${esc(host)}</span>, port <span class="pr-endpoint">${sc.port}</span> and bus <span class="pr-endpoint">${esc(bus)}</span> (or pick BenchBob from the list when the beacon is on). SavvyCAN reconnects if the bus is silent for 10 s; that's its socketcand client, not a fault here.</div></details>
      <details><summary class="pr-note" style="cursor:pointer">WebSocket and REST</summary><pre class="pr-log" style="user-select:all">${esc(wsjs)}</pre><div class="pr-note">Each message is <span class="pr-endpoint">{type:"frame", id, ext, rtr, fd, brs, len, data, ts, dir}</span>. Add <span class="pr-endpoint">batch=1</span> for 50 ms batches and <span class="pr-endpoint">stats=1</span> for the ID table. One-shot capture: <span class="pr-endpoint">curl 'http://${esc(location.host)}/api/can/${esc(bus)}/capture?ms=2000&amp;filter=7E8'</span>. Full list at <a href="/api" target="_blank">/api</a>; agents use the MCP tools at <span class="pr-endpoint">/mcp</span> (can_list, can_send, can_capture…).</div></details>
    </div>`;
  }

  periphRegister('can', {
    mount(el) {
      root = el;
      el.innerHTML = '<div class="can-ifaces"></div><div class="can-tools"></div><div class="can-ep"></div>';
    },
    update(st) {
      if (!root || !st) return;
      S0 = st;
      renderIfaces(st);
      renderTools(st);
      renderEndpoints(st);
    },
    unmount() {
      for (const n of Object.keys(cards)) closeMon(n);
      for (const n of Object.keys(cards)) delete cards[n];
      root = null;
    },
  });
})();
