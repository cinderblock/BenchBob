// GPS panel: pick a receiver, run a temporary gpsd, show fix + sky plot. Everything comes
// from the status stream (~2 s), which is plenty for a 1 Hz receiver.
(() => {
  let el, st = {};
  const PFX = { GPS: 'G', GLONASS: 'R', Galileo: 'E', BeiDou: 'C', SBAS: 'S', QZSS: 'J', NavIC: 'I' };
  const snrColor = s => !s ? '#7a7f94' : s < 20 ? '#ff5c5c' : s < 30 ? '#f5a623' : s < 40 ? '#c7d64f' : '#4caf7d';
  const fmt = (v, d = 1, u = '') => v == null ? '–' : (+v).toFixed(d) + u;

  function skyPlot(sats, size = 240) {
    const c = size / 2, R = c - 16;
    let h = `<svg viewBox="0 0 ${size} ${size}" width="100%" style="max-width:${size}px;display:block">`;
    for (const el of [0, 30, 60]) h += `<circle cx="${c}" cy="${c}" r="${R * (90 - el) / 90}" fill="none" stroke="#2a2f40"/>`;
    h += `<line x1="${c}" y1="${c - R}" x2="${c}" y2="${c + R}" stroke="#2a2f40"/><line x1="${c - R}" y1="${c}" x2="${c + R}" y2="${c}" stroke="#2a2f40"/>`;
    for (const [l, x, y] of [['N', c, 10], ['E', size - 6, c + 4], ['S', c, size - 2], ['W', 6, c + 4]]) h += `<text x="${x}" y="${y}" fill="#7a7f94" font-size="10" text-anchor="middle">${l}</text>`;
    h += `<text x="${c + 3}" y="${c - R * 2 / 3 - 2}" fill="#4a4f60" font-size="8">30°</text><text x="${c + 3}" y="${c - R / 3 - 2}" fill="#4a4f60" font-size="8">60°</text>`;
    for (const s of sats) {
      if (s.el == null || s.az == null || s.el < 0) continue;
      const r = R * (90 - s.el) / 90, a = s.az * Math.PI / 180;
      const x = c + r * Math.sin(a), y = c - r * Math.cos(a), col = snrColor(s.snr);
      h += `<circle cx="${x.toFixed(1)}" cy="${y.toFixed(1)}" r="7" fill="${s.used ? col : 'none'}" stroke="${col}" stroke-width="1.5"/>`;
      h += `<text x="${x.toFixed(1)}" y="${(y + 3).toFixed(1)}" fill="${s.used ? '#0c0e13' : col}" font-size="7" font-weight="700" text-anchor="middle">${esc((PFX[s.gnss] || '?') + (s.svid ?? s.prn))}</text>`;
    }
    return h + '</svg>';
  }

  function mount(e, s) {
    el = e;
    el.innerHTML = `<div data-k="top"></div><div data-k="run"></div><div data-k="log"></div>`;
    el.addEventListener('click', onClick);
    el.addEventListener('change', onChange);
    update(s);
  }

  const opt = k => el.querySelector(`[data-o="${k}"]`);
  function startOpts() {
    return { port: +opt('port')?.value || undefined, share: !!opt('share')?.checked, readOnly: !!opt('ro')?.checked, pps: opt('pps') ? opt('pps').checked : undefined };
  }
  async function onClick(e) {
    const b = e.target.closest('[data-a]'); if (!b) return;
    b.disabled = true;
    try {
      if (b.dataset.a === 'start') await papi('POST', '/api/gps/start', { device: b.dataset.dev, ...startOpts() });
      else if (b.dataset.a === 'path') await papi('POST', '/api/gps/start', { path: opt('path').value.trim(), ...startOpts() });
      else if (b.dataset.a === 'stop') await papi('POST', '/api/gps/stop');
    } catch {} finally { b.disabled = false; }
  }
  function onChange(e) {
    const u = e.target.dataset.use;
    if (u) papi('POST', '/api/gps/use', { device: u, use: e.target.checked }).catch(() => {});
  }

  function update(s) {
    st = s || {};
    if (!el) return;
    renderTop();
    renderRun();
    const lg = el.querySelector('[data-k=log]');
    const lh = st.log?.length ? `<div class="pr-note" style="margin-top:8px">gpsd output</div><div class="pr-log" style="max-height:140px">${st.log.map(l => esc(l.line)).join('\n')}</div>` : '';
    if (lg.dataset.sig !== lh) { lg.dataset.sig = lh; lg.innerHTML = lh; }
  }

  function renderTop() {
    const host = el.querySelector('[data-k=top]');
    const cs = st.candidates || [], set = st.settings || {};
    const sig = JSON.stringify([cs, set, st.running, st.device, st.ppsDevices, st.lastExit]);
    if (host.dataset.sig === sig) return;
    host.dataset.sig = sig;
    let h = '';
    for (const c of cs) {
      const gps = c.kind === 'gps' || c.chosen, mine = st.running && st.device === c.path;
      h += `<div class="pr-card"><h4>${esc(c.path)} <small>${esc([c.vendor, c.model].filter(Boolean).join(' '))} · ${esc(c.usbId)}${c.serial ? ' · SN ' + esc(c.serial) : ''}</small>
        ${c.kind === 'gps' ? `<span class="pr-pill on">${esc(c.label)}</span>` : c.label ? `<span class="pr-pill">${esc(c.label)}</span>` : ''}
        ${mine ? '<span class="pr-pill on">gpsd running</span>' : ''}${c.holders.length ? `<span class="pr-pill warn">open by ${esc(c.holders.map(x => x.comm + ' ' + x.pid).join(', '))}</span>` : ''}</h4>
        <div class="pr-row">${c.kind !== 'gps' ? `<label><input type="checkbox" data-use="${esc(c.key)}" ${c.chosen ? 'checked' : ''}> Use as GPS</label>` : ''}
        ${gps && !mine ? `<button class="btn btn-sm btn-primary" data-a="start" data-dev="${esc(c.key)}" ${c.holders.length ? 'disabled' : ''}>Start gpsd</button>` : ''}
        ${c.holders.length ? '<span class="pr-note">Another program has this port. Close it there (e.g. the Serial panel) to use it here.</span>' : ''}</div></div>`;
    }
    if (!cs.length) h += '<div class="pr-note">No USB serial devices. GPS receivers show up here when plugged in (u-blox and MediaTek are recognised; for other adapters tick "Use as GPS").</div>';
    if (!st.running) {
      h += `<div class="pr-row">
        <label>Port <input data-o="port" type="number" min="1" max="65535" value="${set.port}" style="width:80px"></label>
        <label><input type="checkbox" data-o="share" ${set.share ? 'checked' : ''}> Share on network</label>
        <label><input type="checkbox" data-o="ro" ${set.readOnly ? 'checked' : ''}> Read-only (never configure the receiver)</label>
        ${st.ppsDevices?.length ? `<label><input type="checkbox" data-o="pps" ${set.pps !== false ? 'checked' : ''}> Use ${esc(st.ppsDevices.join(', '))}</label>` : ''}</div>
        <div class="pr-row"><input data-o="path" placeholder="Other tty, e.g. /dev/serial/by-id/…" style="flex:1;min-width:160px"><button class="btn btn-sm" data-a="path">Start on this path</button></div>
        <div class="pr-note">gpsd runs only while started here: a temporary experiment, not a system service.</div>`;
      if (st.lastExit) h += `<div class="pr-row"><span class="pr-pill bad">gpsd exited (${st.lastExit.code})</span> <span class="pr-note">${esc(st.lastExit.message)}</span></div>`;
    }
    host.innerHTML = h;
  }

  function renderRun() {
    const host = el.querySelector('[data-k=run]');
    if (!st.running) { host.innerHTML = ''; return; }
    const f = st.fix || {}, sats = st.satellites || [];
    const pill = f.mode === 3 ? 'on' : f.mode === 2 ? 'warn' : 'bad';
    let h = `<div class="pr-row"><span class="pr-pill ${pill}">${esc(f.fix === 'unknown' ? 'waiting for data' : f.fix)}</span>
      <span class="pr-note">gpsd on ${esc(st.device)} · clients connect to <span class="pr-endpoint">${esc(st.share ? location.hostname + ':' + st.port : st.connect)}</span> · since ${fmtAgo(st.since)}${st.readOnly ? ' · read-only' : ''}</span>
      <button class="btn btn-sm btn-danger" data-a="stop">Stop gpsd</button></div>`;
    if (st.gpsd?.errors?.length) h += `<div class="pr-row"><span class="pr-pill bad">${esc(st.gpsd.errors.at(-1))}</span></div>`;
    const d = st.gpsd?.devices?.[0];
    if (d) h += `<div class="pr-note">${esc(d.driver || 'detecting protocol…')}${d.subtype ? ' · ' + esc(d.subtype) : ''}${d.subtype1 ? ' · ' + esc(d.subtype1) : ''}${d.bps ? ' · ' + d.bps + ' bps' : ''}${st.gpsd.version ? ' · gpsd ' + esc(st.gpsd.version) : ''}</div>`;
    const map = f.lat != null ? ` <a href="https://www.openstreetmap.org/?mlat=${f.lat}&mlon=${f.lon}#map=17/${f.lat}/${f.lon}" target="_blank" rel="noopener">map</a>` : '';
    const rows = [
      ['Position', f.lat != null ? `${f.lat.toFixed(6)}, ${f.lon.toFixed(6)}${map}` : '–'],
      ['Altitude', `${fmt(f.altMSL_m, 1, ' m')} MSL${f.altHAE_m != null ? ` · ${fmt(f.altHAE_m, 1, ' m')} HAE` : ''}`],
      ['Speed / track', `${fmt(f.speed_ms, 2, ' m/s')}${f.speed_ms != null ? ` (${(f.speed_ms * 3.6).toFixed(1)} km/h)` : ''} · ${fmt(f.track_deg, 1, '°')}`],
      ['Accuracy', `±${fmt(f.eph_m, 1, ' m')} horiz${f.epv_m != null ? ` · ±${fmt(f.epv_m, 1, ' m')} vert` : ''}`],
      ['DOP', `HDOP ${fmt(f.hdop, 2)} · PDOP ${fmt(f.pdop, 2)} · VDOP ${fmt(f.vdop, 2)}`],
      ['UTC time', f.time ? esc(f.time) : '–'],
      ['vs system clock', `${f.toffOffset_ms != null ? `${f.toffOffset_ms} ms (gpsd TOFF)` : '–'}${f.nmeaOffset_ms != null ? ` · NMEA arrival ${f.nmeaOffset_ms} ms` : ''} <span class="pr-note">negative = receiver's time is behind; includes serial latency</span>`],
      ['PPS', st.ppsDevices?.length ? (f.ppsOffset_us != null ? `${f.ppsOffset_us} µs offset (${f.ppsAge_s} s ago)` : `${esc(st.ppsDevices.join(', '))}: no pulses yet`) : 'no /dev/pps* device'],
      ['NTP', esc(st.ntp || '')],
    ];
    h += `<div style="display:flex;flex-wrap:wrap;gap:16px;align-items:flex-start;margin-top:8px">
      <div style="flex:0 1 240px;min-width:180px">${skyPlot(sats)}
        <div class="pr-note">Filled = used in fix. Colour = signal: <span style="color:#4caf7d">≥40</span> <span style="color:#c7d64f">30s</span> <span style="color:#f5a623">20s</span> <span style="color:#ff5c5c">&lt;20</span> dB-Hz. G GPS · R GLONASS · E Galileo · C BeiDou · S SBAS · J QZSS</div></div>
      <div style="flex:1 1 260px;min-width:0"><table class="pr-tbl">${rows.map(([k, v]) => `<tr><td class="pr-note" style="white-space:nowrap">${k}</td><td style="word-break:break-word">${v}</td></tr>`).join('')}</table>
        <table class="pr-tbl" style="margin-top:8px"><thead><tr><th>Constellation</th><th>Used</th><th>Visible</th></tr></thead><tbody>
        ${Object.entries(f.constellations || {}).map(([g, c]) => `<tr><td>${esc(g)}</td><td>${c.used}</td><td>${c.visible}</td></tr>`).join('') || '<tr><td colspan="3" class="pr-note">No satellites reported yet</td></tr>'}
        <tr><td><b>Total</b></td><td><b>${f.satsUsed ?? 0}</b></td><td><b>${f.satsVisible ?? 0}</b></td></tr></tbody></table></div></div>`;
    host.innerHTML = h;
  }

  periphRegister('gps', { mount, update, unmount() { el = null; } });
})();
