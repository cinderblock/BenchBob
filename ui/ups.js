// UPS section: one card per UPS with big charge/runtime/load figures, decoded status pills,
// charge & load sparklines (10 minutes), details, the power-event log, and the NUT
// network-sharing switch. Rendered from the status push alone; nothing to fetch.
(() => {
  let root = null, lastSig = '';

  // Linear line sparkline, 0–100 scale (both series are percentages).
  function spark(vals, color) {
    const w = 240, h = 36;
    if (!vals || vals.length < 2) return `<svg width="100%" height="${h}" viewBox="0 0 ${w} ${h}" preserveAspectRatio="none"></svg>`;
    const n = 120, dx = w / (n - 1), off = n - vals.length;
    const pts = vals.map((v, i) => `${((off + i) * dx).toFixed(1)},${(h - 2 - (Math.max(0, Math.min(100, v)) / 100) * (h - 4)).toFixed(1)}`).join(' ');
    return `<svg width="100%" height="${h}" viewBox="0 0 ${w} ${h}" preserveAspectRatio="none" style="display:block"><polyline points="${pts}" fill="none" stroke="${color}" stroke-width="1.5" vector-effect="non-scaling-stroke"/></svg>`;
  }

  const big = (label, value, sub) => `<div style="flex:1;min-width:90px"><div class="pr-note">${label}</div><div style="font-size:1.4rem;font-weight:600;font-variant-numeric:tabular-nums">${value}</div>${sub ? `<div class="pr-note">${sub}</div>` : ''}</div>`;
  const v = (x, unit) => x == null ? '—' : `${x}${unit}`;

  function card(u) {
    const pills = u.flags.map(f => `<span class="pr-pill ${f.severity === 'ok' ? 'on' : f.severity === 'warn' ? 'warn' : f.severity === 'bad' ? 'bad' : ''}">${esc(f.word)}</span>`).join(' ');
    const state = !u.driverRunning ? '<span class="pr-pill bad">NUT driver not running</span>' : !u.reachable ? '<span class="pr-pill warn">waiting for data</span>' : pills;
    const rows = [
      ['Input', [u.inputVoltage != null ? `${u.inputVoltage} V` : '', u.inputFrequency != null ? `${u.inputFrequency} Hz` : ''].filter(Boolean).join(' · ')],
      ['Output', u.outputVoltage != null ? `${u.outputVoltage} V` : ''],
      ['Power', u.realpower != null ? `${u.realpower} W` : ''],
      ['Battery', [u.batteryVoltage != null ? `${u.batteryVoltage} V` : '', u.batteryType, u.batteryDate ? `dated ${u.batteryDate}` : ''].filter(Boolean).join(' · ')],
      ['Low battery at', [u.lowCharge != null ? `${u.lowCharge}%` : '', u.lowRuntime != null ? `${u.lowRuntime} s runtime` : ''].filter(Boolean).join(' or ')],
      ['Serial', u.serial], ['Firmware', u.firmware], ['Beeper', u.beeper],
      ['Connection', u.simulated ? 'Simulated (dummy-ups)' : `USB ${u.usb} · NUT ${u.nutDriver}`],
    ].filter(r => r[1]);
    return `<div class="pr-card">
      <h4>${esc(u.vendor)} ${esc(u.model)} <small>NUT name ${esc(u.name)}</small>${u.simulated ? ' <span class="pr-pill warn">simulated</span>' : ''}</h4>
      <div class="pr-row">${state}</div>
      ${u.error ? `<div class="pr-note" style="color:var(--danger)">${esc(u.error)}</div>` : ''}
      <div class="pr-row" style="gap:16px">
        ${big('Charge', v(u.charge, '%'), spark(u.history.charge, 'var(--success)'))}
        ${big('Runtime', esc(u.runtimeText || '—'), '')}
        ${big('Load', v(u.load, '%'), spark(u.history.load, 'var(--accent)'))}
      </div>
      <div class="pr-note">Sparklines: last 10 minutes, 0–100%.</div>
      <table class="pr-tbl" style="margin-top:8px">${rows.map(r => `<tr><td class="pr-note" style="width:9em">${r[0]}</td><td>${esc(r[1])}</td></tr>`).join('')}</table>
      ${u.events.length ? `<div class="pr-note" style="margin-top:8px">Power events</div><div class="pr-log" style="max-height:140px">${u.events.slice().reverse().map(e => `${esc(new Date(e.t).toLocaleString())}  ${esc(e.text)}`).join('\n')}</div>` : '<div class="pr-note" style="margin-top:8px">No power events since DeskyPi started.</div>'}
    </div>`;
  }

  function nutHtml(n, any) {
    return `<div class="pr-card">
      <h4>NUT server <small>${n.running ? 'running' : any ? 'stopped' : 'starts when a UPS is connected'}</small></h4>
      <div class="pr-row">
        <label><input type="checkbox" class="ups-share" ${n.shared ? 'checked' : ''} onchange="upsShare()"> Share on network, port</label>
        <input type="number" class="ups-port" min="1" max="65535" value="${n.port}" style="width:6em" onchange="upsShare()">
        <button class="btn btn-sm" onclick="papi('POST','/api/ups/restart')">Restart NUT</button>
      </div>
      <div class="pr-note">${n.shared
        ? `NUT clients elsewhere can monitor <span class="pr-endpoint">deskypi@${esc(location.hostname)}:${n.port}</span>. For upsmon: <span class="pr-endpoint">MONITOR deskypi@${esc(location.hostname)}:${n.port} 1 ${esc(n.user)} ${esc(n.password || '')} secondary</span>`
        : `Listening on 127.0.0.1:${n.port} only (DeskyPi itself).`}</div>
      <div class="pr-note">Automatic shutdown of DeskyPi on low battery is <b>not configured</b>.</div>
      ${n.log?.length ? `<div class="pr-log">${n.log.map(esc).join('\n')}</div>` : ''}
    </div>`;
  }

  window.upsShare = () => {
    const share = root.querySelector('.ups-share').checked, port = Number(root.querySelector('.ups-port').value);
    papi('POST', '/api/ups/share', { share, port }).catch(() => {});
  };

  periphRegister('ups', {
    mount(el) { root = el; lastSig = ''; },
    update(st) {
      if (!root) return;
      // Don't rebuild while the port box is being edited.
      if (root.contains(document.activeElement) && document.activeElement.classList.contains('ups-port')) return;
      const sig = JSON.stringify(st);
      if (sig === lastSig) return;
      lastSig = sig;
      root.innerHTML = (st.upses.length ? st.upses.map(card).join('') : '<div class="pr-note">No UPS connected. Plug a UPS in by USB (APC, CyberPower, Eaton, Tripp Lite, Liebert and anything NUT supports).</div>') + nutHtml(st.nut, st.upses.length > 0);
    },
    unmount() { root = null; },
  });
})();
