// RTL-SDR panel: sticks + DVB-driver check, mode switch (rtl_tcp / rtl_433 / off), and the
// rtl_433 sensor table, which is polled from /api/sdr/rtl433 while it has anything to show.
(() => {
  let el, st = {}, poll = null, data = null, seq = 0, msgs = [];
  const SR = [[250000, '250 kS/s'], [1024000, '1.024 MS/s'], [1800000, '1.8 MS/s'], [2048000, '2.048 MS/s'], [2400000, '2.4 MS/s'], [3200000, '3.2 MS/s (may drop samples)']];
  const META = new Set(['type', 'subtype']);

  function line(pts, w = 70, h = 18, color = '#ee8a6c') {
    if (!pts || pts.length < 2) return '';
    const vs = pts.map(p => p[1]), mn = Math.min(...vs), mx = Math.max(...vs), span = mx - mn || 1;
    const t0 = pts[0][0], tspan = pts[pts.length - 1][0] - t0 || 1;
    const p = pts.map(([t, v]) => `${((t - t0) / tspan * w).toFixed(1)},${(h - 1 - (v - mn) / span * (h - 2)).toFixed(1)}`).join(' ');
    return `<svg width="${w}" height="${h}" viewBox="0 0 ${w} ${h}" style="display:inline-block;vertical-align:middle"><polyline points="${p}" fill="none" stroke="${color}" stroke-width="1.2"/></svg>`;
  }

  function mount(e, s) {
    el = e;
    el.innerHTML = `<div data-k="sticks"></div><div data-k="mode"></div><div data-k="forms"></div><div data-k="r433"></div>`;
    el.addEventListener('click', onClick);
    update(s);
  }

  const val = k => el.querySelector(`[data-f="${k}"]`)?.value;
  async function onClick(e) {
    const b = e.target.closest('[data-a]'); if (!b) return;
    const a = b.dataset.a, stick = b.dataset.stick || undefined;
    const force = el.querySelector('[data-f=force]')?.checked || undefined;
    b.disabled = true;
    try {
      if (a === 'probe') await papi('POST', '/api/sdr/probe', { stick });
      else if (a === 'off') await papi('POST', '/api/sdr/mode', { mode: 'off' });
      else if (a === 'tcp') await papi('POST', '/api/sdr/mode', { mode: 'rtl_tcp', force, port: +val('port'), bind: val('bind'), gain: +val('tgain') || 0, ppm: +val('tppm') || 0, sampleRate: +val('sr') });
      else if (a === '433') await papi('POST', '/api/sdr/mode', { mode: 'rtl_433', force, preset: val('preset'), hop: +val('hop') || undefined, gain: val('rgain') || '', ppm: +val('rppm') || 0 });
      else if (a === 'clear') { await papi('DELETE', '/api/sdr/rtl433'); seq = 0; msgs = []; data = null; refresh(); }
    } catch {} finally { b.disabled = false; }
  }

  function update(s) {
    st = s || {};
    if (!el) return;
    renderSticks();
    renderMode();
    renderForms();
    const want = st.mode === 'rtl_433' || st.rtl433?.sensors > 0;
    if (want && !poll) { refresh(); poll = setInterval(refresh, 2000); }
    else if (!want && poll) { clearInterval(poll); poll = null; data = null; renderR433(); }
  }

  function renderSticks() {
    const host = el.querySelector('[data-k=sticks]');
    const d = st.dvb || {};
    let h = '';
    for (const s of st.sticks || []) {
      const p = s.probe;
      h += `<div class="pr-card"><h4>${esc(s.manufacturer || '')} ${esc(s.product || s.knownAs)} <small>${esc(s.usbId)} · ${esc(s.knownAs)}</small>
        ${s.dvbClaimed ? '<span class="pr-pill bad">held by DVB driver</span>' : ''}</h4>
        <div class="pr-note">Serial ${esc(s.serial || '–')} · USB ${esc(s.bus)} · port ${esc(s.port || '–')}${s.drivers.length ? ' · driver ' + esc(s.drivers.join(', ')) : ''}</div>
        <div class="pr-note">${p ? `Tuner <b>${esc(p.tuner || 'unknown')}</b>${p.gains?.length ? ` · ${p.gains.length} gain steps 0–${p.gains.at(-1)} dB` : ''}${p.eeprom ? ` · EEPROM: ${esc(p.eeprom.Manufacturer || '')} / ${esc(p.eeprom.Product || '')} / SN ${esc(p.eeprom['Serial number'] || '')}` : ''}${p.error ? ` <span class="pr-pill bad">${esc(p.error)}</span>` : ''} · probed ${fmtAgo(p.t)}` : 'Not probed yet'}
        <button class="btn btn-sm" data-a="probe" data-stick="${esc(s.key)}" ${st.mode !== 'off' || st.probing ? 'disabled' : ''}>${st.probing ? 'Probing…' : 'Probe tuner'}</button></div>
      </div>`;
    }
    if (!(st.sticks || []).length) h += `<div class="pr-note">No RTL2832U stick detected (known USB ids: 0bda:2838, 0bda:2832 and clones).</div>`;
    h += `<div class="pr-row pr-note">DVB driver ${esc('dvb_usb_rtl28xxu')}: ${d.blacklisted ? `<span class="pr-pill on">blacklisted</span> ${esc(d.where?.[0] || '')}` : `<span class="pr-pill warn">not blacklisted</span>${d.loaded ? ' <span class="pr-pill bad">module loaded</span>' : ''} A stick plugged in now would be grabbed by the TV driver. Fix: <span class="pr-endpoint">${esc(d.fix)}</span>`}</div>`;
    if (host.dataset.sig !== h) { host.dataset.sig = h; host.innerHTML = h; }
  }

  function renderMode() {
    const host = el.querySelector('[data-k=mode]');
    let h = '<div class="pr-row">';
    if (st.mode === 'off') {
      h += '<span class="pr-pill">Off</span>';
      if (st.lastExit) h += ` <span class="pr-pill bad">${esc(st.lastExit.mode)} exited (${st.lastExit.code})</span> <span class="pr-note">${esc(st.lastExit.message)}</span>`;
    } else {
      h += `<span class="pr-pill on">${esc(st.mode)}</span> <span class="pr-note">since ${fmtAgo(st.since)}</span> <button class="btn btn-sm btn-danger" data-a="off">Stop</button>`;
    }
    h += '</div>';
    if (st.mode === 'rtl_tcp') {
      const port = st.opts?.port, hn = location.hostname;
      h += `<div class="pr-card"><h4>rtl_tcp server <small>${esc(st.opts?.bind)}:${port} · ${(st.opts?.sampleRate / 1e6).toFixed(3)} MS/s · gain ${st.opts?.gain ? esc(st.opts.gain) + ' dB' : 'auto'} · ${st.opts?.ppm} ppm</small></h4>
        <div class="pr-note">SDR++: source <b>RTL-TCP</b>, host <span class="pr-endpoint">${esc(hn)}</span> port <span class="pr-endpoint">${port}</span></div>
        <div class="pr-note">GQRX device string: <span class="pr-endpoint">rtl_tcp=${esc(hn)}:${port}</span></div>
        <div class="pr-note">SDR#: source "RTL-SDR (TCP)", host ${esc(hn)}, port ${port}</div>
        <div class="pr-note">Client: ${st.clients?.length ? st.clients.map(c => `<span class="pr-pill on">${esc(c)}</span>`).join(' ') : 'none connected (rtl_tcp serves one client at a time)'}</div></div>`;
    }
    if (host.dataset.sig !== h) { host.dataset.sig = h; host.innerHTML = h; }
  }

  function renderForms() {
    const host = el.querySelector('[data-k=forms]');
    const sig = JSON.stringify([st.mode, (st.sticks || []).length, st.defaults]);
    if (host.dataset.sig === sig) return;
    host.dataset.sig = sig;
    if (st.mode !== 'off') { host.innerHTML = ''; return; }
    const t = st.defaults?.settings?.tcp || {}, r = st.defaults?.settings?.rtl433 || {};
    const noStick = !(st.sticks || []).length;
    host.innerHTML = `
      <div class="pr-card"><h4>rtl_tcp server <small>raw I/Q for SDR++, GQRX, SDR#</small></h4><div class="pr-row">
        <label>Port <input data-f="port" type="number" min="1" max="65535" value="${t.port || st.defaults?.tcpPort || 1234}" style="width:80px"></label>
        <label>Listen <select data-f="bind"><option value="0.0.0.0">all interfaces</option><option value="127.0.0.1" ${t.bind === '127.0.0.1' ? 'selected' : ''}>this Pi only</option></select></label>
        <label>Rate <select data-f="sr">${SR.map(([v, l]) => `<option value="${v}" ${(t.sampleRate || 2048000) === v ? 'selected' : ''}>${l}</option>`).join('')}</select></label>
        <label>Gain dB <input data-f="tgain" type="number" step="0.1" min="0" max="50" value="${t.gain || ''}" placeholder="auto" style="width:64px"></label>
        <label>PPM <input data-f="tppm" type="number" value="${t.ppm || 0}" style="width:56px"></label>
        <button class="btn btn-sm btn-primary" data-a="tcp">Start rtl_tcp</button></div></div>
      <div class="pr-card"><h4>rtl_433 <small>decode ISM-band sensors</small></h4><div class="pr-row">
        <label>Band <select data-f="preset">${Object.entries(st.presets || {}).map(([k, p]) => `<option value="${esc(k)}" ${(r.preset || '433') === k ? 'selected' : ''}>${esc(p.label)}</option>`).join('')}</select></label>
        <label>Hop every <input data-f="hop" type="number" min="5" value="${r.hop || 60}" style="width:56px"> s</label>
        <label>Gain dB <input data-f="rgain" type="number" step="0.1" min="0" max="50" value="${esc(r.gain || '')}" placeholder="auto" style="width:64px"></label>
        <label>PPM <input data-f="rppm" type="number" value="${r.ppm || 0}" style="width:56px"></label>
        <button class="btn btn-sm btn-primary" data-a="433">Start rtl_433</button></div></div>
      ${noStick ? '<div class="pr-row"><label><input type="checkbox" data-f="force"> Try anyway (a clone with an unlisted USB id)</label></div>' : ''}`;
  }

  async function refresh() {
    try {
      const r = await fetch('/api/sdr/rtl433?since=' + seq).then(r => r.json());
      if (r.seq < seq) msgs = [];
      msgs = msgs.concat(r.messages).slice(-100);
      seq = r.seq; data = r;
      renderR433();
    } catch {}
  }

  function fieldStr(k, v) {
    if (k === 'battery_ok') return v >= 1 ? 'battery ok' : '<span class="pr-pill bad">battery low</span>';
    if (k === 'temperature_C') return `<b>${esc(v)} °C</b>`;
    if (k === 'humidity') return `<b>${esc(v)} %RH</b>`;
    return `${esc(k)} ${esc(typeof v === 'object' ? JSON.stringify(v) : v)}`;
  }

  function renderR433() {
    const host = el.querySelector('[data-k=r433]');
    if (!data) { host.innerHTML = ''; return; }
    let h = `<div class="pr-row"><b style="font-size:.85rem">Decoded sensors</b> <span class="pr-note">${data.devices.length} sensors · ${data.seq} messages</span><button class="btn btn-sm" data-a="clear">Clear</button></div>`;
    if (!data.devices.length) h += '<div class="pr-note">Nothing decoded yet. Most sensors transmit every 30–60 s.</div>';
    else {
      h += '<table class="pr-tbl"><thead><tr><th>Sensor</th><th>Latest</th><th>Signal</th><th>Seen</th></tr></thead><tbody>';
      for (const d of data.devices) {
        const f = Object.entries(d.fields).filter(([k]) => !META.has(k));
        const sp = d.hist.temperature_C ? line(d.hist.temperature_C) : d.hist.humidity ? line(d.hist.humidity, 70, 18, '#6cb4ee') : d.hist.moisture ? line(d.hist.moisture, 70, 18, '#4caf7d') : '';
        h += `<tr><td style="word-break:break-word"><b>${esc(d.model)}</b>${d.fields.type ? ` <span class="pr-pill">${esc(d.fields.type)}</span>` : ''}<br><span class="pr-note">id ${esc(d.id || '–')}${d.channel ? ' · ch ' + esc(d.channel) : ''}${d.protocol ? ' · proto ' + d.protocol : ''}</span></td>
          <td style="word-break:break-word">${f.map(([k, v]) => fieldStr(k, v)).join(' · ')}${sp ? '<br>' + sp : ''}</td>
          <td style="white-space:nowrap">${d.rssi != null ? d.rssi.toFixed(1) + ' dB' : '–'}${d.snr != null ? `<br><span class="pr-note">SNR ${d.snr.toFixed(1)}</span>` : ''}${d.freq ? `<br><span class="pr-note">${d.freq.toFixed(3)} MHz</span>` : ''}</td>
          <td style="white-space:nowrap">${fmtAgo(d.lastSeen)}<br><span class="pr-note">${d.count}×</span></td></tr>`;
      }
      h += '</tbody></table>';
    }
    h += `<div class="pr-note" style="margin-top:8px">Raw messages (newest last)</div><div class="pr-log" style="max-height:200px">${msgs.map(m => esc(new Date(m.t).toLocaleTimeString() + ' ' + JSON.stringify(m.msg))).join('\n') || '–'}</div>`;
    if (data.log?.length) h += `<div class="pr-note" style="margin-top:8px">rtl_433 log</div><div class="pr-log" style="max-height:120px">${data.log.map(l => esc(l.line)).join('\n')}</div>`;
    const log = host.querySelector('.pr-log'), atEnd = !log || log.scrollTop + log.clientHeight >= log.scrollHeight - 4;
    host.innerHTML = h;
    const nl = host.querySelector('.pr-log'); if (nl && atEnd) nl.scrollTop = nl.scrollHeight;
  }

  periphRegister('sdr', { mount, update, unmount() { if (poll) clearInterval(poll); poll = null; el = null; data = null; msgs = []; seq = 0; } });
})();
