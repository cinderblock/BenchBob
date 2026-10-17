// Wired network section: per-interface link card (with the switch port it's plugged into),
// iperf3 server/client with a live throughput graph, and quick checks. Forms are built once
// in mount() so typing isn't disturbed; the result areas re-render only when their slice
// of the status changes.
(() => {
  let root = null;
  const sigs = {};
  const part = (cls, html, key) => { const el = root.querySelector(cls); if (el && sigs[cls] !== key) { sigs[cls] = key; el.innerHTML = html; } };
  const fmtBps = b => { if (!b) return '0 bit/s'; const u = ['bit/s', 'kbit/s', 'Mbit/s', 'Gbit/s']; const i = Math.min(Math.floor(Math.log10(b) / 3), 3); const v = b / 10 ** (i * 3); return `${v.toFixed(i ? (v < 10 ? 2 : 1) : 0)} ${u[i]}`; };
  const pill = (t, cls = '') => `<span class="pr-pill ${cls}">${esc(t)}</span>`;
  const sp = s => s == null ? '' : s >= 1000 ? `${s / 1000} Gb/s` : `${s} Mb/s`;
  // "1000baseT/Full 100baseT/Full" → "1000/Full 100/Full": shorter, same meaning on a phone.
  const modes = m => m?.length ? m.map(x => x.replace('baseT', '').replace(/base(\w+)/, ' $1')).join(' · ') : '—';

  function neighborHtml(n, lldp) {
    if (!lldp.enabled) return '<div class="pr-note">Switch info: listening is off.</div>';
    if (!n) return `<div class="pr-note">Switch info: ${lldp.running ? `listening… (${Math.round((Date.now() - lldp.since) / 1000)} s; switches announce about every 30 s)` : esc(lldp.error || 'lldpd not running')}</div>`;
    const vl = n.vlans.map(v => `${v.id}${v.name ? ` ${v.name}` : ''}${v.pvid ? ' (untagged)' : ''}`).join(', ');
    const caps = n.capabilities.filter(c => c.enabled).map(c => c.type).join(', ');
    const poe = n.poe ? Object.entries(n.poe).map(([k, v]) => `${k} ${v}`).join(' · ') : '';
    const rows = [
      ['Switch', `${n.name || ''}${n.description ? ` — ${n.description}` : ''}`],
      ['Port', `${n.portDescription || n.portId}${n.portDescription && n.portId !== n.portDescription ? ` (id ${n.portId})` : ''}`],
      ['Chassis', `${n.chassisId} (${n.chassisIdType})`],
      ['VLAN', vl], ['PoE', poe], ['Management', n.mgmtIps.join(', ')], ['Capabilities', caps],
      ['Heard via', `${n.protocol} · ${n.age.replace(/^0 day, /, '')} ago-ish · TTL ${n.ttl ?? '?'} s${n.medDeviceType ? ` · ${n.medDeviceType}` : ''}`],
    ].filter(r => r[1]);
    return `<table class="pr-tbl">${rows.map(r => `<tr><td class="pr-note" style="width:7.5em">${r[0]}</td><td>${esc(r[1])}</td></tr>`).join('')}</table>`;
  }

  function ifaceHtml(i, st) {
    const l = i.link || {};
    const head = i.carrier
      ? `${pill(`${sp(l.speed)} ${l.duplex || ''}`.trim() || 'link up', 'on')} ${l.autoneg === false ? pill('autoneg off', 'warn') : ''} ${l.duplex === 'Half' ? pill('half duplex', 'warn') : ''}`
      : pill('no link', 'bad');
    const errs = i.errors.length
      ? `<table class="pr-tbl"><tr><th>Error counter</th><th>Total</th><th>Since DeskyPi started</th></tr>${i.errors.map(e => `<tr><td>${esc(e.name)}</td><td>${e.value}</td><td>${e.delta ? `<span style="color:var(--warning)">+${e.delta}</span>` : '0'}</td></tr>`).join('')}</table>`
      : '<div class="pr-note">No error counters above zero.</div>';
    const eee = i.eee ? (i.eee.supported ? `${i.eee.enabled ? 'enabled' : 'disabled'}, ${i.eee.active ? 'active' : 'inactive'}${i.eee.partner.length ? '' : ' (partner advertises none)'}` : 'not supported') : '—';
    const addrs = i.addrs.filter(a => a.family === 'inet' || !a.addr.startsWith('fe80')).map(a => `${a.addr}/${a.prefix}`).join(', ') || '—';
    return `<div class="pr-card">
      <h4>${esc(i.name)} ${head} ${i.usb ? pill('USB') : ''}<small>${esc(i.usbDesc)}</small></h4>
      <table class="pr-tbl">
        <tr><td class="pr-note" style="width:7.5em">Addresses</td><td>${esc(addrs)}</td></tr>
        <tr><td class="pr-note">Gateway / DNS</td><td>${esc(i.gateway || '—')} / ${esc((i.dns.length ? i.dns : st.dns.nameservers).join(', ') || '—')}</td></tr>
        <tr><td class="pr-note">Partner offers</td><td>${esc(modes(l.partner))}${l.partnerAutoneg === false ? ' (no autoneg)' : ''}</td></tr>
        <tr><td class="pr-note">We advertise</td><td>${esc(modes(l.advertised))}</td></tr>
        <tr><td class="pr-note">EEE</td><td>${esc(eee)}</td></tr>
        <tr><td class="pr-note">Hardware</td><td>${esc([i.driver, i.firmware && `fw ${i.firmware}`, i.bus, i.mac, `MTU ${i.mtu}`].filter(Boolean).join(' · '))}</td></tr>
        <tr><td class="pr-note">Carrier changes</td><td>${i.carrierChanges}${l.masterSlave ? ` · PHY ${esc(l.masterSlave)}` : ''}</td></tr>
      </table>
      ${errs}
      <div style="margin-top:8px">${neighborHtml(i.neighbor, st.lldp)}</div>
    </div>`;
  }

  // Throughput over time, filled area + line, with the peak labelled.
  function graph(points, duration) {
    const w = 320, h = 90;
    if (!points.length) return `<svg viewBox="0 0 ${w} ${h}" width="100%" height="${h}"></svg>`;
    const max = Math.max(...points.map(p => p.bps), 1);
    const tmax = Math.max(duration, points[points.length - 1].t);
    const x = t => (t / tmax) * (w - 2) + 1, y = b => h - 14 - (b / max) * (h - 24);
    const pts = [[0, points[0].bps], ...points.map(p => [p.t, p.bps])];
    const line = pts.map(([t, b]) => `${x(t).toFixed(1)},${y(b).toFixed(1)}`).join(' ');
    return `<svg viewBox="0 0 ${w} ${h}" width="100%" height="${h}" preserveAspectRatio="none" style="display:block;background:var(--surface3);border-radius:4px">
      <polygon points="${x(0)},${h - 14} ${line} ${x(pts[pts.length - 1][0])},${h - 14}" fill="var(--accent)" opacity=".18"/>
      <polyline points="${line}" fill="none" stroke="var(--accent)" stroke-width="1.5" vector-effect="non-scaling-stroke"/>
      <text x="4" y="11" font-size="9" fill="var(--dim)">peak ${fmtBps(max)}</text>
      <text x="${w - 4}" y="${h - 3}" font-size="9" fill="var(--dim)" text-anchor="end">${tmax.toFixed(0)} s</text>
    </svg>`;
  }

  function runsHtml(runs) {
    if (!runs.length) return '<div class="pr-note">No client runs yet.</div>';
    const cur = runs[0];
    const last = cur.points[cur.points.length - 1];
    const now = cur.status === 'running'
      ? `<div class="pr-row">${pill('running', 'on')} ${esc(cur.host)} ${last ? fmtBps(last.bps) : 'connecting…'} <button class="btn btn-sm" onclick="papi('POST','/api/netdiag/iperf/${cur.id}/stop')">Stop</button></div>`
      : cur.status === 'error' ? `<div class="pr-note" style="color:var(--danger)">${esc(cur.host)}: ${esc(cur.error)}</div>` : '';
    const res = r => r.result ? `${fmtBps(r.result.bps)}${r.result.retransmits != null ? ` · ${r.result.retransmits} retr` : ''}${r.result.jitterMs != null ? ` · jitter ${r.result.jitterMs.toFixed(2)} ms · loss ${r.result.lostPct}%` : ''}` : r.status === 'running' ? '…' : esc(r.error);
    return `${now}${graph(cur.points, cur.duration)}
      <table class="pr-tbl" style="margin-top:6px"><tr><th>When</th><th>Host</th><th>Test</th><th>Result</th></tr>${runs.map(r => `<tr><td>${fmtAgo(r.started)}</td><td>${esc(r.host)}:${r.port}</td><td>${r.udp ? 'UDP' : 'TCP'}${r.reverse ? ' ↓ (reverse)' : ' ↑'} ${r.duration}s${r.parallel > 1 ? ` ×${r.parallel}` : ''}${r.bitrate ? ` @${esc(r.bitrate)}` : ''}</td><td>${res(r)}</td></tr>`).join('')}</table>
      <div class="pr-note">↑ DeskyPi sends · ↓ DeskyPi receives</div>`;
  }

  function serverHtml(s) {
    const off = s.offAt ? ` · turns off in ${Math.max(0, Math.round((s.offAt - Date.now()) / 60000))} min` : '';
    const head = s.running ? `${pill('listening', 'on')} on port ${s.port}: run <span class="pr-endpoint">iperf3 -c ${esc(location.hostname)} -p ${s.port}</span> from another machine${off}` : `<span class="pr-note">Off${s.log.length ? ` (${esc(s.log[s.log.length - 1])})` : ''}</span>`;
    const cur = s.current ? `<div class="pr-row">${pill('test running', 'on')} from ${esc(s.current.peer)} · ${esc(s.current.protocol)}${s.current.reverse ? ' reverse' : ''} · ${fmtBps(s.current.bps)}</div>` : '';
    const tests = s.tests.length ? `<table class="pr-tbl"><tr><th>When</th><th>Client</th><th>Test</th><th>Result</th></tr>${s.tests.map(t => `<tr><td>${fmtAgo(t.t)}</td><td>${esc(t.peer)}</td><td>${t.summary ? `${esc(t.summary.protocol)}${t.summary.reverse ? ' (we sent)' : ' (we received)'}` : ''}</td><td>${t.summary ? `${fmtBps(t.summary.bps)}${t.summary.retransmits != null ? ` · ${t.summary.retransmits} retr` : ''}${t.summary.jitterMs != null ? ` · jitter ${t.summary.jitterMs.toFixed(2)} ms · loss ${t.summary.lostPct}%` : ''}` : esc(t.error)}</td></tr>`).join('')}</table>` : '';
    return `<div class="pr-row">${head}</div>${cur}${tests}`;
  }

  function checksHtml(c) {
    let h = '';
    if (c.ping) { const p = c.ping; h += `<div class="pr-row"><b>Ping ${esc(p.host)}</b> ${p.received}/${p.sent} replies, ${p.lossPct}% loss${p.avg != null ? ` · ${p.min}/${p.avg}/${p.max} ms (min/avg/max) · jitter ${p.mdev} ms` : ''} <span class="pr-note">${fmtAgo(p.t)}</span></div>`; }
    if (c.dns) h += `<div style="margin-top:6px"><b>DNS</b> <span class="pr-note">${fmtAgo(c.dns.t)}</span><table class="pr-tbl">${c.dns.results.map(r => `<tr><td>${esc(r.server)}</td><td>${esc(r.name)}</td><td>${r.ms != null ? `${r.ms} ms` : `<span style="color:var(--danger)">${esc(r.error)}</span>`}</td><td class="pr-note">${esc(r.addresses.slice(0, 2).join(', '))}</td></tr>`).join('')}</table></div>`;
    if (c.http) h += `<div style="margin-top:6px"><b>HTTP</b> <span class="pr-note">${fmtAgo(c.http.t)}</span><table class="pr-tbl">${c.http.results.map(r => `<tr><td style="word-break:break-all">${esc(r.url)}</td><td>${r.status != null ? r.status : `<span style="color:var(--danger)">${esc(r.error)}</span>`}</td><td>${r.ms != null ? `${r.ms} ms` : ''}</td></tr>`).join('')}</table></div>`;
    if (c.mtu) h += `<div class="pr-row"><b>Path MTU to ${esc(c.mtu.host)}</b> ${c.mtu.pathMtu ?? '?'} (interface ${c.mtu.interfaceMtu}) · ${esc(c.mtu.note)} <span class="pr-note">${c.mtu.steps.length} probes, ${fmtAgo(c.mtu.t)}</span></div>`;
    return h || '<div class="pr-note">Run a check above.</div>';
  }

  // Buttons share one "busy" state so a slow MTU probe doesn't get double-clicked.
  async function busy(btn, fn) {
    btn.disabled = true; const t = btn.textContent; btn.textContent = t + '…';
    try { await fn(); } catch {} finally { btn.disabled = false; btn.textContent = t; }
  }
  window.ndCheck = (btn, kind) => busy(btn, () => {
    const host = root.querySelector('.nd-chost').value.trim();
    const b = kind === 'ping' ? { host: host || undefined } : kind === 'mtu' ? { host: host || undefined } : {};
    return papi('POST', `/api/netdiag/${kind}`, b);
  });
  window.ndRun = btn => busy(btn, () => {
    const q = s => root.querySelector(s);
    return papi('POST', '/api/netdiag/iperf', { host: q('.nd-host').value.trim(), port: Number(q('.nd-port').value), duration: Number(q('.nd-dur').value), reverse: q('.nd-rev').checked, udp: q('.nd-udp').checked, bitrate: q('.nd-bw').value.trim() || undefined, parallel: Number(q('.nd-par').value) });
  });
  window.ndServer = on => papi('POST', '/api/netdiag/iperf/server', { enabled: on, port: Number(root.querySelector('.nd-sport').value) }).catch(() => {});
  window.ndLldp = on => papi('POST', '/api/netdiag/lldp', { enabled: on }).catch(() => {});

  periphRegister('netdiag', {
    mount(el, st) {
      root = el;
      for (const k in sigs) delete sigs[k];
      el.innerHTML = `
        <div class="nd-ifaces"></div>
        <div class="pr-row"><label><input type="checkbox" class="nd-lldp" onchange="ndLldp(this.checked)"> Listen for switch info (LLDP/CDP, receive-only: DeskyPi never announces itself)</label></div>
        <div class="pr-card"><h4>iperf3 client <small>from DeskyPi</small></h4>
          <div class="pr-row">
            <label>Host <input class="nd-host" placeholder="server IP" style="width:10em"></label>
            <label>Port <input class="nd-port" type="number" value="5201" style="width:5em"></label>
            <label>Seconds <input class="nd-dur" type="number" value="10" min="1" max="60" style="width:4em"></label>
            <label>Streams <input class="nd-par" type="number" value="1" min="1" max="16" style="width:3.5em"></label>
          </div>
          <div class="pr-row">
            <label><input type="checkbox" class="nd-rev"> Reverse (server sends)</label>
            <label><input type="checkbox" class="nd-udp"> UDP</label>
            <label>Bitrate <input class="nd-bw" placeholder="e.g. 100M" style="width:6em"></label>
            <button class="btn btn-sm btn-primary" onclick="ndRun(this)">Run</button>
          </div>
          <div class="nd-runs"></div>
        </div>
        <div class="pr-card"><h4>iperf3 server <small>temporary; turns itself off</small></h4>
          <div class="pr-row">
            <label>Port <input class="nd-sport" type="number" style="width:5em"></label>
            <button class="btn btn-sm nd-son" onclick="ndServer(true)">Start</button>
            <button class="btn btn-sm nd-soff" onclick="ndServer(false)">Stop</button>
          </div>
          <div class="nd-server"></div>
        </div>
        <div class="pr-card"><h4>Quick checks</h4>
          <div class="pr-row">
            <label>Host <input class="nd-chost" placeholder="gateway / 1.1.1.1" style="width:10em"></label>
            <button class="btn btn-sm" onclick="ndCheck(this,'ping')">Ping</button>
            <button class="btn btn-sm" onclick="ndCheck(this,'dns')">DNS timing</button>
            <button class="btn btn-sm" onclick="ndCheck(this,'http')">HTTP(S)</button>
            <button class="btn btn-sm" onclick="ndCheck(this,'mtu')">Path MTU</button>
          </div>
          <div class="pr-note">Ping defaults to the gateway (10 pings); Path MTU defaults to 1.1.1.1.</div>
          <div class="nd-checks"></div>
        </div>`;
      el.querySelector('.nd-sport').value = st?.iperf?.server?.port || 5201;
    },
    update(st) {
      if (!root) return;
      const ifKey = JSON.stringify([st.ifaces, st.lldp, st.dns, Math.floor(Date.now() / 5000)]);
      part('.nd-ifaces', st.ifaces.length ? st.ifaces.map(i => ifaceHtml(i, st)).join('') : '<div class="pr-note">No wired interface found.</div>', ifKey);
      const cb = root.querySelector('.nd-lldp'); if (cb && document.activeElement !== cb) cb.checked = st.lldp.enabled;
      part('.nd-runs', runsHtml(st.iperf.runs), JSON.stringify(st.iperf.runs) + Math.floor(Date.now() / 30000));
      part('.nd-server', serverHtml(st.iperf.server), JSON.stringify(st.iperf.server) + Math.floor(Date.now() / 30000));
      root.querySelector('.nd-son').style.display = st.iperf.server.running ? 'none' : '';
      root.querySelector('.nd-soff').style.display = st.iperf.server.running ? '' : 'none';
      part('.nd-checks', checksHtml(st.checks), JSON.stringify(st.checks) + Math.floor(Date.now() / 30000));
    },
    unmount() { root = null; },
  });
})();
