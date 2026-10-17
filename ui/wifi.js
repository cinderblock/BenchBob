// ==================== Wi-Fi (peripherals/drivers/wifi.ts) ====================
// Radios + regulatory domain, a scanner with per-band channel-occupancy charts and a
// sortable BSS table, and netns client tests with a live timeline. Status carries only
// sequence numbers for the bulky data; this fetches /api/wifi/{radios,scan,results} when
// one changes, and re-renders only the block that changed so form inputs keep their state.
(() => {
  const W = {
    el: null, st: null,
    seq: { radios: -1, scan: -1, results: -1, profiles: -1 },
    radios: null, scan: null, results: [], profiles: [],
    sort: { key: 'signal', dir: -1 }, group: false, band: '', open: new Set(), openRes: new Set(), showChans: new Set(),
  };

  // Styles that only this section needs; the rest reuse .pr-* from index.html.
  if (!document.getElementById('wf-css')) {
    const s = document.createElement('style');
    s.id = 'wf-css';
    s.textContent = `
.wf-blk{margin-top:14px}.wf-blk>h4{font-size:.7rem;font-weight:700;text-transform:uppercase;letter-spacing:.08em;color:var(--dim);margin-bottom:6px;display:flex;gap:8px;align-items:center;flex-wrap:wrap}
.wf-scroll{overflow-x:auto;-webkit-overflow-scrolling:touch}
.wf-chart{display:block;min-width:560px;width:100%;height:auto;background:#0a0c11;border:1px solid var(--border);border-radius:6px;margin-top:6px}
.wf-chart text{font-family:system-ui,sans-serif}
.wf-chans{display:flex;flex-wrap:wrap;gap:3px;margin-top:6px}
.wf-ch{font-size:.68rem;font-family:'SF Mono',ui-monospace,monospace;padding:1px 5px;border-radius:4px;background:var(--surface3);color:var(--text)}
.wf-ch.off{color:var(--dim);text-decoration:line-through;opacity:.6}
.wf-ch.dfs{background:rgba(245,166,35,.15);color:var(--warning)}
.wf-tbl th{cursor:pointer;white-space:nowrap}.wf-tbl th.on{color:var(--accent)}
.wf-tbl td{white-space:nowrap}.wf-tbl tr.wf-r{cursor:pointer}.wf-tbl tr.wf-r:hover td{background:var(--surface2)}
.wf-tbl tr.wf-g td{background:var(--surface2);font-weight:600}
.wf-tbl .wf-d td{white-space:normal;background:#10131a;font-size:.75rem;color:var(--dim)}
.wf-tbl .wf-d b{color:var(--text);font-weight:500}
.wf-dim{color:var(--dim)}.wf-mono{font-family:'SF Mono',ui-monospace,monospace}
.wf-tl{font-size:.75rem;font-family:'SF Mono',ui-monospace,monospace;display:grid;grid-template-columns:auto auto 1fr;gap:1px 8px;margin-top:6px}
.wf-tl .ok{color:var(--success)}.wf-tl .bad{color:var(--danger)}.wf-tl .t{color:var(--dim);text-align:right}
.wf-form{display:grid;grid-template-columns:repeat(auto-fill,minmax(210px,1fr));gap:8px 12px;margin-top:6px}
.wf-form label{display:flex;flex-direction:column;gap:2px;font-size:.72rem;color:var(--dim)}
.wf-form label.chk{flex-direction:row;align-items:center;gap:6px}
.wf-form input,.wf-form select{background:var(--surface2);border:1px solid var(--border);color:var(--text);border-radius:4px;padding:4px 6px;font:inherit;font-size:.8rem;min-width:0}
.wf-res{border-top:1px solid var(--border);padding:6px 0;font-size:.8rem}
.wf-res .hd{display:flex;gap:6px 10px;flex-wrap:wrap;align-items:baseline;cursor:pointer}
@media(max-width:600px){.wf-hide-sm{display:none}}
`;
    document.head.appendChild(s);
  }

  // ---------- helpers ----------
  const $ = (sel) => W.el.querySelector(sel);
  const hue = (s) => { let h = 0; for (const c of s || '?') h = (h * 31 + c.charCodeAt(0)) >>> 0; return h % 360; };
  const ssidColor = (b) => (b.hidden ? '#7a7f94' : `hsl(${hue(b.ssid)},70%,62%)`);
  const ssidLabel = (b) => (b.hidden ? '(hidden)' : b.ssid);
  const bandName = { '2.4': '2.4 GHz', '5': '5 GHz', '6': '6 GHz', '60': '60 GHz' };
  const secPill = (b) => `<span class="pr-pill ${b.securityKind === 'open' || b.securityKind === 'wep' ? 'bad' : b.securityKind === 'psk' ? '' : 'on'}">${esc(b.security)}</span>`;
  const sig = (dbm) => (dbm == null ? '<span class="wf-dim">?</span>' : `<span style="color:${wifiColor(dbm)};font-weight:600">${dbm}</span>`);
  const secs = (ms) => (ms / 1000).toFixed(1) + 's';

  function spark(hist, w = 54, h = 14) {
    const d = (hist || []).slice(-30);
    if (!d.length) return '';
    const bw = w / 30;
    let bars = '';
    d.forEach((p, i) => {
      const v = Array.isArray(p) ? p[1] : p;
      const bh = Math.max(1, Math.min(1, Math.max(0, (v + 95) / 65)) * h);
      bars += `<rect x="${(30 - d.length + i) * bw}" y="${h - bh}" width="${Math.max(bw - 0.4, 0.5)}" height="${bh}" fill="${wifiColor(v)}" rx="0.4"/>`;
    });
    return `<svg width="${w}" height="${h}" viewBox="0 0 ${w} ${h}" style="display:inline-block;vertical-align:middle">${bars}</svg>`;
  }

  async function load(what) {
    try {
      if (what === 'radios') W.radios = await papi('GET', '/api/wifi/radios');
      if (what === 'scan') W.scan = await papi('GET', '/api/wifi/scan');
      if (what === 'results') W.results = (await papi('GET', '/api/wifi/results')).results;
      if (what === 'profiles') W.profiles = (await papi('GET', '/api/wifi/profiles')).profiles;
    } catch { return; }
    if (!W.el) return;
    if (what === 'radios') { renderRadios(); renderCharts(); syncForm(); }
    if (what === 'scan') { renderScanMeta(); renderCharts(); renderTable(); syncForm(); }
    if (what === 'results') renderResults();
    if (what === 'profiles') syncForm();
  }

  // ---------- radios + regulatory ----------
  function bandSummary(b) {
    const on = b.channels.filter((c) => !c.disabled);
    const off = b.channels.length - on.length;
    const dfs = on.filter((c) => c.radar).length;
    const pw = Math.max(0, ...on.map((c) => c.maxPower || 0));
    return `<b>${bandName[b.band]}</b> ${on.length} channels${off ? ` (+${off} disabled)` : ''}${dfs ? `, ${dfs} DFS` : ''}, up to ${b.maxWidth} MHz${pw ? `, ≤${pw} dBm` : ''} · ${[b.ht && 'n', b.vht && 'ac', b.he && 'ax', b.eht && 'be'].filter(Boolean).join('/') || 'legacy'}`;
  }

  function renderRadios() {
    const R = W.radios;
    if (!R) return;
    let h = '';
    for (const r of R.radios) {
      const ifn = r.ifaces.filter((i) => i.name).map((i) => i.name).join(', ') || 'no interface';
      const where = r.onboard ? `Onboard (${r.bus.toUpperCase()})` : r.bus === 'virtual' ? 'Virtual (mac80211_hwsim)' : `USB: ${esc([r.usb?.vendor, r.usb?.model].filter(Boolean).join(' ') || r.usb?.id || '?')}${r.usb ? ` <span class="wf-dim wf-mono">${esc(r.usb.id)}</span>` : ''}`;
      const kind = r.use.kind;
      const pill = kind === 'primary' ? 'warn' : kind === 'backup' || kind === 'connected' ? 'on' : kind === 'test' || kind === 'scan' ? 'warn' : '';
      const nmCtl = !r.onboard && r.nm.length && r.use.kind !== 'test'
        ? (r.unmanagedByUs.length
          ? `<button class="btn btn-sm" data-act="managed" data-phy="${r.phy}" data-v="1">Give back to NetworkManager</button>`
          : r.nm.some((n) => n.state === 'connected') ? '' : `<button class="btn btn-sm" data-act="managed" data-phy="${r.phy}" data-v="0">Keep NetworkManager off it</button>`)
        : '';
      h += `<div class="pr-card">
        <h4>${esc(r.label || r.phy)} <small>${esc(ifn)} · ${esc(r.driver || '?')} · ${where}</small><span class="pr-pill ${pill}">${esc(r.use.text)}</span></h4>
        <div class="pr-note">${r.bandInfo.map(bandSummary).join('<br>') || 'No band information'}</div>
        <div class="pr-note">Modes: ${esc(r.iftypes.join(', '))}${r.monitor ? '' : ' · <b>no monitor mode</b>'}${r.sae ? ' · WPA3-SAE capable' : ''}${r.netns ? '' : ' · <b>can’t move to a netns (no client tests)</b>'}${r.regCountry ? ` · own regulatory domain ${esc(r.regCountry)}` : ''}</div>
        ${r.unmanagedByUs.length ? `<div class="pr-note" style="color:var(--warning)">DeskyPi told NetworkManager to leave ${esc(r.unmanagedByUs.join(', '))} alone (until reboot) so it can’t auto-connect while DeskyPi uses it.</div>` : ''}
        <div class="pr-row"><button class="btn btn-sm" data-act="chans" data-phy="${r.phy}">${W.showChans.has(r.phy) ? 'Hide channels' : 'Show channels'}</button>${nmCtl}</div>
        ${W.showChans.has(r.phy) ? r.bandInfo.map((b) => `<div class="pr-note" style="margin-top:6px">${bandName[b.band]}: <span class="wf-ch">enabled</span> <span class="wf-ch dfs">DFS/radar</span> <span class="wf-ch off">disabled</span></div><div class="wf-chans">${b.channels.map((c) => `<span class="wf-ch ${c.disabled ? 'off' : c.radar ? 'dfs' : ''}">${c.ch}${c.maxPower && !c.disabled ? `<span class="wf-dim"> ${c.maxPower}</span>` : ''}${c.noIR && !c.disabled ? '<span class="wf-dim"> passive</span>' : ''}</span>`).join('')}</div>`).join('') : ''}
      </div>`;
    }
    if (!R.radios.length) h = '<div class="pr-note">No Wi-Fi radios found.</div>';
    const g = R.reg;
    h += `<div class="pr-row" style="margin-top:10px"><span class="pr-note">Regulatory domain: <b style="color:var(--text)">${esc(g.country || '?')}</b>${g.dfsRegion ? ` (${esc(g.dfsRegion)})` : ''}${g.changedFrom != null ? ` <span class="pr-pill warn">temporary, was ${esc(g.changedFrom)}</span>` : ''}${g.phys.map((p) => ` · ${esc(p.scope)}: ${esc(p.country)}${p.selfManaged ? ' (self-managed)' : ''}`).join('')}</span>
      <label>Country <input class="wf-reg" size="3" maxlength="2" placeholder="US"></label><button class="btn btn-sm" data-act="reg">Set until reboot</button></div>
      <div class="pr-note wf-regnote"></div>
      ${R.ouiSource ? '' : '<div class="pr-note">No OUI vendor database installed, so the vendor column is empty (apt package <span class="wf-mono">ieee-data</span> provides one).</div>'}`;
    $('.wf-radios').innerHTML = h;
  }

  // ---------- scanner ----------
  function renderScanMeta() {
    const s = W.scan?.scan;
    const st = W.st?.scan;
    const auto = st?.auto || {};
    const busy = st?.scanning?.length;
    const radios = W.radios?.radios || [];
    // Rebuild the controls only when they'd change: a status push every 2 s would otherwise
    // close a dropdown the user has open.
    const ctl = $('.wf-scanctl');
    const sigc = JSON.stringify([busy, auto, radios.map((r) => r.phy + r.label), W.group, W.band]);
    if (ctl.dataset.sig !== sigc) { ctl.dataset.sig = sigc; ctl.innerHTML = `
      <select class="wf-scanradio">${radios.map((r) => `<option value="${r.phy}" ${auto.radio === r.phy ? 'selected' : ''}>${esc(r.label || r.phy)} (${esc(r.ifaces.find((i) => i.name)?.name || '-')})</option>`).join('')}</select>
      <button class="btn btn-sm btn-primary" data-act="scan" ${busy ? 'disabled' : ''}>${busy ? 'Scanning…' : 'Scan now'}</button>
      <label class="pr-note"><input type="checkbox" class="wf-auto" ${auto.enabled ? 'checked' : ''}> every</label>
      <select class="wf-autoint">${[15, 30, 60, 120, 300, 600].map((v) => `<option value="${v}" ${auto.intervalS === v ? 'selected' : ''}>${v < 60 ? v + ' s' : v / 60 + ' min'}</option>`).join('')}</select>
      <label class="pr-note"><input type="checkbox" class="wf-group" ${W.group ? 'checked' : ''}> group by SSID</label>
      <select class="wf-band"><option value="">all bands</option>${['2.4', '5', '6'].map((b) => `<option value="${b}" ${W.band === b ? 'selected' : ''}>${bandName[b]}</option>`).join('')}</select>
      <button class="btn btn-sm" data-act="clearscan">Forget history</button>`; }
    let note = '';
    if (s?.at) note = `Last scan ${fmtAgo(s.at)} on ${esc(s.iface || s.radio)} via ${esc(s.method === 'nm' ? 'NetworkManager rescan + iw scan dump' : 'iw scan')} in ${secs(s.durationMs)}: ${s.count} BSS · ${W.scan.bss.length} remembered (1 h)`;
    if (s?.error) note += ` · <span style="color:var(--danger)">${esc(s.error)}</span>`;
    if (s?.at) note += s.survey ? '' : ` · channel survey: not supported by this driver`;
    $('.wf-scanmeta').innerHTML = note || 'No scan yet. Scans don’t disturb an existing connection.';
    const sv = s?.survey;
    $('.wf-survey').innerHTML = sv ? `<details><summary>Channel survey: ${sv.length} channels, busiest ${Math.max(...sv.map((x) => x.busyPct ?? 0))}%</summary><div class="wf-scroll"><table class="pr-tbl"><thead><tr><th>Freq</th><th>Ch</th><th>Busy</th><th>Noise</th><th>Active</th></tr></thead><tbody>${sv.map((x) => `<tr><td>${x.freq}${x.inUse ? ' <span class="pr-pill on">in use</span>' : ''}</td><td>${chOf(x.freq)}</td><td>${x.busyPct != null ? x.busyPct + '%' : '-'}</td><td>${x.noise ?? '-'} dBm</td><td>${x.activeMs ?? '-'} ms</td></tr>`).join('')}</tbody></table></div></details>` : '';
  }

  const chOf = (f) => (f === 2484 ? 14 : f < 2500 ? (f - 2407) / 5 : f === 5935 ? 2 : f >= 5950 ? (f - 5950) / 5 : (f - 5000) / 5);

  // Charts: x is frequency (MHz) mapped through segments, y is signal (-95..-25 dBm).
  // Every BSS is a trapezoid "hump" spanning its whole (possibly bonded) channel.
  const SEGS = {
    '2.4': [{ a: 2400, b: 2495, name: '' }],
    '5': [{ a: 5170, b: 5250, name: 'U-NII-1' }, { a: 5250, b: 5330, name: 'U-NII-2A', dfs: true }, { a: 5490, b: 5730, name: 'U-NII-2C', dfs: true }, { a: 5735, b: 5835, name: 'U-NII-3' }, { a: 5835, b: 5895, name: 'U-NII-4' }],
    '6': [{ a: 5925, b: 6425, name: 'U-NII-5' }, { a: 6425, b: 6525, name: 'U-NII-6' }, { a: 6525, b: 6875, name: 'U-NII-7' }, { a: 6875, b: 7125, name: 'U-NII-8' }],
  };

  function chart(band, list, radioChans) {
    const VW = 640, top = 22, bottom = 26, H = 190, left = 30, right = 8, gap = 10;
    let segs = SEGS[band];
    if (band === '5' && !list.some((b) => b.freq > 5845) && !radioChans.some((c) => c.mhz > 5845 && !c.disabled)) segs = segs.slice(0, 4);
    const total = segs.reduce((n, s) => n + (s.b - s.a), 0);
    const pxPerMHz = (VW - left - right - gap * (segs.length - 1)) / total;
    const xs = [];
    let x0 = left;
    for (const s of segs) { xs.push(x0); x0 += (s.b - s.a) * pxPerMHz + gap; }
    const fx = (f) => {
      for (let i = 0; i < segs.length; i++) if (f <= segs[i].b || i === segs.length - 1) return xs[i] + (Math.max(segs[i].a, Math.min(segs[i].b, f)) - segs[i].a) * pxPerMHz;
      return left;
    };
    const fy = (dbm) => top + (1 - Math.max(0, Math.min(1, (dbm + 95) / 70))) * (H - top - bottom);
    const yb = H - bottom;
    let g = '';
    segs.forEach((s, i) => {
      const w = (s.b - s.a) * pxPerMHz;
      g += `<rect x="${xs[i]}" y="${top}" width="${w}" height="${yb - top}" fill="${s.dfs ? 'rgba(245,166,35,.06)' : 'rgba(255,255,255,.015)'}"/>`;
      if (s.name) g += `<text x="${xs[i] + w / 2}" y="${top - 8}" fill="${s.dfs ? '#f5a623' : '#7a7f94'}" font-size="9" text-anchor="middle">${s.name}${s.dfs ? ' · DFS' : ''}</text>`;
    });
    for (const d of [-90, -80, -70, -60, -50, -40, -30]) {
      g += `<line x1="${left}" x2="${VW - right}" y1="${fy(d)}" y2="${fy(d)}" stroke="#2a2f40" stroke-width="0.5"/><text x="${left - 3}" y="${fy(d) + 3}" fill="#7a7f94" font-size="8" text-anchor="end">${d}</text>`;
    }
    // Channel ticks, greyed where the regulatory domain disables them for this radio.
    const ticks = band === '2.4' ? [...Array(14)].map((_, i) => i + 1) : band === '5' ? [36, 40, 44, 48, 52, 56, 60, 64, 100, 104, 108, 112, 116, 120, 124, 128, 132, 136, 140, 144, 149, 153, 157, 161, 165, 169, 173, 177] : [1, 17, 33, 49, 65, 81, 97, 113, 129, 145, 161, 177, 193, 209, 225];
    for (const ch of ticks) {
      const f = band === '2.4' ? (ch === 14 ? 2484 : 2407 + ch * 5) : band === '5' ? 5000 + ch * 5 : 5950 + ch * 5;
      if (!segs.some((s) => f >= s.a && f <= s.b)) continue;
      const rc = radioChans.find((c) => c.ch === ch);
      const off = radioChans.length && (!rc || rc.disabled);
      const x = fx(f);
      g += `<line x1="${x}" x2="${x}" y1="${yb}" y2="${yb + 3}" stroke="#7a7f94"/><text x="${x}" y="${yb + 13}" fill="${off ? '#454a5c' : rc?.radar ? '#f5a623' : '#aab'}" font-size="${band === '2.4' ? 9 : 8}" text-anchor="middle"${off ? ' text-decoration="line-through"' : ''}>${ch}</text>`;
    }
    g += `<text x="${VW - right}" y="${H - 3}" fill="#7a7f94" font-size="8" text-anchor="end">channel · dBm</text>`;
    // Humps, weakest first so strong networks draw on top.
    const sorted = [...list].sort((a, b) => (a.signal ?? -100) - (b.signal ?? -100));
    for (const b of sorted) {
      if (b.signal == null) continue;
      const w = b.width === '80+80' ? 80 : b.width;
      const xa = fx(b.centerFreq - w / 2), xz = fx(b.centerFreq + w / 2), yp = fy(b.signal), sl = Math.min(6, (xz - xa) / 4);
      const c = ssidColor(b);
      g += `<path d="M${xa},${yb} L${xa + sl},${yp} L${xz - sl},${yp} L${xz},${yb}" fill="${c}" fill-opacity="0.08" stroke="${c}" stroke-width="${b.associated ? 2 : 1.1}" stroke-opacity="0.9"${b.hidden ? ' stroke-dasharray="3,2"' : ''}/>`;
    }
    // Label the strongest BSS per SSID+centre, the 10 strongest, staggered to limit overlaps.
    const seen = new Set();
    const labels = [];
    for (const b of [...sorted].reverse()) {
      const k = ssidLabel(b) + '@' + b.centerFreq;
      if (seen.has(k) || b.signal == null) continue;
      seen.add(k);
      labels.push(b);
      if (labels.length >= 10) break;
    }
    const placed = [];
    for (const b of labels) {
      const x = fx(b.centerFreq);
      let y = fy(b.signal) - 3;
      while (placed.some((p) => Math.abs(p.x - x) < 70 && Math.abs(p.y - y) < 10)) y += 10;
      placed.push({ x, y });
      g += `<text x="${x}" y="${y}" fill="${ssidColor(b)}" font-size="9" text-anchor="middle">${esc(ssidLabel(b).slice(0, 22))}</text>`;
    }
    const empty = list.length ? '' : `<text x="${VW / 2}" y="${(top + yb) / 2}" fill="#7a7f94" font-size="11" text-anchor="middle">nothing seen on ${bandName[band]}</text>`;
    return `<svg class="wf-chart" viewBox="0 0 ${VW} ${H}" role="img" aria-label="${bandName[band]} channel occupancy">${g}${empty}</svg>`;
  }

  function renderCharts() {
    if (!W.scan) return;
    const all = W.scan.bss.filter((b) => Date.now() - b.lastSeen < 10 * 60 * 1000); // recent only
    const radio = (W.radios?.radios || []).find((r) => r.phy === (W.scan.scan.radio || '')) || W.radios?.radios?.[0];
    const chans = (band) => radio?.bandInfo.find((b) => b.band === band)?.channels || [];
    const has6 = (W.radios?.radios || []).some((r) => r.bands.includes('6')) || all.some((b) => b.band === '6');
    let h = '';
    for (const band of ['2.4', '5', ...(has6 ? ['6'] : [])]) {
      if (W.band && W.band !== band) continue;
      const list = all.filter((b) => b.band === band);
      h += `<div class="pr-note" style="margin-top:8px"><b style="color:var(--text)">${bandName[band]}</b> · ${list.length} BSS${radio && !chans(band).length ? ` · ${esc(radio.phy)} doesn’t support this band` : ''}</div><div class="wf-scroll">${chart(band, list, chans(band))}</div>`;
    }
    if (!has6 && !W.band) h += '<div class="pr-note">No 6 GHz chart: no radio here supports 6 GHz.</div>';
    $('.wf-charts').innerHTML = h;
  }

  const COLS = [
    ['ssid', 'SSID'], ['bssid', 'BSSID', 'wf-hide-sm'], ['channel', 'Ch'], ['width', 'Width'], ['signal', 'Signal'], ['security', 'Security'], ['std', 'Std', 'wf-hide-sm'], ['load', 'Load', 'wf-hide-sm'], ['seen', 'Seen'],
  ];
  const sortVal = (b, k) => (k === 'ssid' ? (b.hidden ? '￿' : b.ssid.toLowerCase()) : k === 'channel' ? b.freq : k === 'width' ? (b.width === '80+80' ? 160 : b.width) : k === 'signal' ? b.signal ?? -200 : k === 'security' ? b.security : k === 'std' ? b.standards.length : k === 'load' ? b.utilization ?? -1 : k === 'seen' ? b.lastSeen : b.bssid);

  function row(b, indent) {
    const age = Date.now() - b.lastSeen;
    const r = `<tr class="wf-r" data-bssid="${b.bssid}"${age > 120000 ? ' style="opacity:.55"' : ''}>
      <td>${indent ? '&nbsp;&nbsp;' : ''}<span style="color:${ssidColor(b)}">●</span> ${b.hidden ? '<i class="wf-dim">hidden</i>' : esc(b.ssid)}${b.associated ? ' <span class="pr-pill on">connected</span>' : ''}</td>
      <td class="wf-mono wf-hide-sm">${b.bssid}${b.vendor ? `<br><span class="wf-dim">${esc(b.vendor)}</span>` : ''}</td>
      <td>${b.channel} <span class="wf-dim">${b.band}</span></td>
      <td>${b.width}${b.width !== 20 ? `<span class="wf-dim"> @${b.centerChannel}</span>` : ''}</td>
      <td>${spark(b.history)} ${sig(b.signal)}</td>
      <td>${secPill(b)}</td>
      <td class="wf-hide-sm">${esc(b.standards.join('/'))}</td>
      <td class="wf-hide-sm">${b.stations != null ? `${b.stations} sta` : ''}${b.utilization != null ? ` · ${b.utilization}%` : ''}</td>
      <td class="wf-dim">${fmtAgo(b.lastSeen)}</td></tr>`;
    if (!W.open.has(b.bssid)) return r;
    const d = [
      `<b>BSSID</b> ${b.bssid}${b.localMac ? ' (locally administered: a virtual AP / extra SSID, so no vendor)' : ''}${b.vendor ? ` · ${esc(b.vendor)}` : ''}`,
      `<b>Frequency</b> ${b.freq} MHz, channel ${b.channel}, ${b.width} MHz wide centred on ${b.centerFreq} MHz (ch ${b.centerChannel})`,
      `<b>Security</b> ${esc(b.security)} · AKM ${esc(b.akm.join(', ') || 'none')} · pairwise ${esc(b.pairwise.join(', ') || '-')} · group ${esc(b.group || '-')} · PMF ${b.pmf}`,
      `<b>Standards</b> ${esc(b.standards.join(', '))}${b.generation ? ` (${b.generation})` : ''} · beacon ${b.beaconInterval ?? '?'} TU · ${esc(b.capability.join(' '))}`,
      b.stations != null || b.utilization != null ? `<b>BSS load</b> ${b.stations ?? '?'} stations, channel ${b.utilization ?? '?'}% busy` : '',
      b.country ? `<b>Country IE</b> ${esc(b.country)}` : '',
      b.wps ? `<b>WPS</b> ${esc(b.wps.state || 'present')}${b.wps.locked ? ', AP setup locked' : ''}${b.wps.manufacturer ? ` · ${esc(b.wps.manufacturer)}` : ''}${b.wps.model ? ` ${esc(b.wps.model)}` : ''}${b.wps.device ? ` · “${esc(b.wps.device)}”` : ''}` : '<b>WPS</b> not advertised',
      `<b>Seen</b> first ${fmtAgo(b.firstSeen)}, last ${fmtAgo(b.lastSeen)} by ${esc(b.radio)} · ${b.history.length} samples, ${b.history.length ? `min ${Math.min(...b.history.map((x) => x[1]))} / max ${Math.max(...b.history.map((x) => x[1]))} dBm` : ''}`,
      b.ssidHex ? `<b>Raw SSID</b> <span class="wf-mono">${b.ssidHex}</span>` : '',
      `<button class="btn btn-sm" data-act="usebss" data-bssid="${b.bssid}">Test this network</button>`,
    ].filter(Boolean);
    return r + `<tr class="wf-d"><td colspan="9">${d.join('<br>')}</td></tr>`;
  }

  function renderTable() {
    if (!W.scan) return;
    let list = W.scan.bss.filter((b) => !W.band || b.band === W.band);
    const { key, dir } = W.sort;
    const cmp = (a, b) => { const x = sortVal(a, key), y = sortVal(b, key); return (x < y ? -1 : x > y ? 1 : 0) * dir; };
    list.sort(cmp);
    let body = '';
    if (W.group) {
      const groups = new Map();
      for (const b of list) { const k = b.hidden ? '\u0000hidden' : b.ssid; (groups.get(k) || groups.set(k, []).get(k)).push(b); }
      const gs = [...groups.values()].sort((a, b) => cmp(a[0], b[0]));
      for (const g of gs) {
        const best = Math.max(...g.map((b) => b.signal ?? -200));
        body += `<tr class="wf-g"><td colspan="9"><span style="color:${ssidColor(g[0])}">●</span> ${g[0].hidden ? '<i>hidden</i>' : esc(g[0].ssid)} <span class="wf-dim">· ${g.length} BSS · best ${best} dBm · ch ${[...new Set(g.map((b) => b.channel))].sort((a, b) => a - b).join(', ')} · ${esc([...new Set(g.map((b) => b.security))].join(' / '))}</span></td></tr>`;
        body += g.map((b) => row(b, true)).join('');
      }
    } else body = list.map((b) => row(b)).join('');
    const head = COLS.map(([k, t, cls]) => `<th class="${cls || ''} ${key === k ? 'on' : ''}" data-sort="${k}">${t}${key === k ? (dir > 0 ? ' ▲' : ' ▼') : ''}</th>`).join('');
    $('.wf-table').innerHTML = list.length ? `<div class="wf-scroll"><table class="pr-tbl wf-tbl"><thead><tr>${head}</tr></thead><tbody>${body}</tbody></table></div><div class="pr-note">Tap a row for details. Dashed humps are hidden SSIDs; thick ones are networks a radio is connected to; faded rows weren’t seen in the last 2 minutes.</div>` : '';
  }

  // ---------- client test ----------
  function syncForm() {
    const f = $('.wf-form');
    if (!f) return;
    const radios = W.radios?.radios || [];
    const sel = f.querySelector('[name=radio]');
    const cur = sel.value;
    sel.innerHTML = radios.map((r) => `<option value="${r.phy}" ${r.netns ? '' : 'disabled'}>${esc(r.label || r.phy)} · ${esc(r.ifaces.find((i) => i.name)?.name || '-')} · ${esc(r.use.kind)}</option>`).join('');
    if (cur && radios.some((r) => r.phy === cur)) sel.value = cur;
    const ssids = [...new Set((W.scan?.bss || []).filter((b) => !b.hidden).map((b) => b.ssid))].sort();
    $('#wf-ssids').innerHTML = ssids.map((s) => `<option value="${esc(s)}">`).join('');
    const ps = f.querySelector('[name=profile]');
    const pv = ps.value;
    ps.innerHTML = '<option value="">(none)</option>' + W.profiles.map((p) => `<option value="${p.id}">${esc(p.name)} · ${esc(p.ssid)}${p.hasPsk ? ' · key saved' : p.useSavedKey ? ' · NM key' : ''}</option>`).join('');
    ps.value = pv;
    formVis();
  }

  function formVis() {
    const f = $('.wf-form');
    const v = (n) => f.querySelector(`[name=${n}]`);
    const sec = v('security').value;
    const ssid = v('ssid').value;
    const saved = (W.radios?.savedNetworks || []).some((n) => n.ssid === ssid);
    f.querySelector('.wf-keyrow').style.display = sec === 'open' || sec === 'owe' ? 'none' : '';
    f.querySelector('.wf-nmkey').style.display = saved && sec !== 'open' && sec !== 'owe' && sec !== 'eap' ? '' : 'none';
    f.querySelectorAll('.wf-eap').forEach((e) => (e.style.display = sec === 'eap' ? '' : 'none'));
    f.querySelectorAll('.wf-static').forEach((e) => (e.style.display = v('ip').value === 'static' ? '' : 'none'));
    const r = (W.radios?.radios || []).find((x) => x.phy === v('radio').value);
    let warn = '';
    if (r && (r.use.kind === 'backup' || r.use.kind === 'connected')) warn = `${esc(r.ifaces.find((i) => i.name)?.name)} is in use (${esc(r.use.text)}). The test moves it into its own network namespace, so that connection drops for the test (~15 s plus the test time). NetworkManager reconnects it afterwards, and DeskyPi checks that it does.`;
    if (r && r.use.kind === 'primary') warn = `<span style="color:var(--danger)">${esc(r.ifaces.find((i) => i.name)?.name)} is the primary uplink; DeskyPi will refuse to borrow it.</span>`;
    if (r && !r.onboard) warn = `DeskyPi tells NetworkManager to leave ${esc(r.ifaces.find((i) => i.name)?.name || r.phy)} alone while it uses it (runtime only).`;
    f.querySelector('.wf-warn').innerHTML = warn;
  }

  function readForm() {
    const f = $('.wf-form');
    const v = (n) => f.querySelector(`[name=${n}]`);
    const list = (n) => v(n).value.split(/[\s,]+/).filter(Boolean);
    const o = {
      radio: v('radio').value, ssid: v('ssid').value, security: v('security').value, bssid: v('bssid').value || undefined,
      ip: v('ip').value, pingGateway: v('pingGateway').checked, dns: list('dns'), http: list('http'), ping: list('ping'),
      count: +v('count').value || 5, holdS: +v('holdS').value || 0,
    };
    if (v('profile').value) o.profile = v('profile').value;
    if (v('psk').value) o.psk = v('psk').value;
    if (v('useSavedKey').checked && f.querySelector('.wf-nmkey').style.display !== 'none') o.useSavedKey = true;
    if (o.security === 'eap') o.eap = { method: v('eapMethod').value, identity: v('eapIdentity').value };
    if (o.ip === 'static') o.static = { address: v('sAddr').value, gateway: v('sGw').value || undefined, dns: v('sDns').value.split(/[\s,]+/).filter(Boolean) };
    const ip = v('iperf').value.trim();
    if (ip) { const [server, port] = ip.split(':'); o.iperf = { server, port: +port || 5201, seconds: +v('iperfS').value || 5, reverse: v('iperfR').checked }; }
    if (v('saveAs').value.trim()) o.saveAs = v('saveAs').value.trim();
    return o;
  }

  function loadProfile(id) {
    const p = W.profiles.find((x) => x.id === id);
    if (!p) return;
    const f = $('.wf-form');
    const set = (n, val) => { const e = f.querySelector(`[name=${n}]`); if (e.type === 'checkbox') e.checked = !!val; else e.value = val ?? ''; };
    set('ssid', p.ssid); set('security', p.security || 'psk'); set('bssid', p.bssid); set('ip', p.ip || 'dhcp');
    set('pingGateway', p.pingGateway !== false); set('dns', (p.dns || []).join(', ')); set('http', (p.http || []).join(', ')); set('ping', (p.ping || []).join(', '));
    set('count', p.count || 5); set('holdS', p.holdS || 0); set('useSavedKey', p.useSavedKey); set('psk', '');
    f.querySelector('[name=psk]').placeholder = p.hasPsk ? 'saved in profile' : '';
    if (p.static) { set('sAddr', p.static.address); set('sGw', p.static.gateway); set('sDns', (p.static.dns || []).join(', ')); }
    if (p.iperf) { set('iperf', p.iperf.server + (p.iperf.port && p.iperf.port !== 5201 ? ':' + p.iperf.port : '')); set('iperfS', p.iperf.seconds); set('iperfR', p.iperf.reverse); }
    formVis();
  }

  function timeline(steps) {
    return `<div class="wf-tl">${steps.map((s) => `<span class="t">+${secs(s.dt)}</span><span class="${s.ok === true ? 'ok' : s.ok === false ? 'bad' : 'wf-dim'}">${s.ok === true ? '✓' : s.ok === false ? '✗' : '·'}</span><span>${esc(s.text)}</span>`).join('')}</div>`;
  }

  function renderRunning() {
    const run = W.st?.running || [];
    $('.wf-running').innerHTML = run.map((t) => `<div class="pr-card"><h4>Testing ${esc(t.ssid)} on ${esc(t.phy)} <small>${esc(t.stage)} · started ${fmtAgo(t.startedAt)}</small>
      ${t.link?.signal != null ? `<small>${spark(t.samples.map((x) => x.signal).filter((x) => x != null))} ${sig(t.link.signal)} dBm · tx ${esc(t.link.txRate || '?')} · rx ${esc(t.link.rxRate || '?')}</small>` : ''}
      <button class="btn btn-sm btn-danger" data-act="cancel" data-id="${t.id}">Cancel</button></h4>${timeline(t.timeline)}</div>`).join('');
    const btn = $('[data-act=run]');
    if (btn) btn.disabled = false;
  }

  function renderResults() {
    const R = W.results || [];
    $('.wf-results').innerHTML = R.length ? R.map((r) => {
      const gw = r.tests.find((t) => t.kind === 'ping-gateway');
      const open = W.openRes.has(r.id);
      return `<div class="wf-res"><div class="hd" data-act="res" data-id="${r.id}">
        <span class="pr-pill ${r.ok ? 'on' : 'bad'}">${r.ok ? 'pass' : r.cancelled ? 'cancelled' : 'fail'}</span>
        <b>${esc(r.ssid)}</b><span class="wf-dim">${esc(r.phy)} · ${esc(r.security)} · ${fmtAgo(r.startedAt)} · ${secs((r.endedAt || r.startedAt) - r.startedAt)}</span>
        ${r.link?.signal != null ? `<span>${sig(r.link.signal)} dBm</span>` : ''}${r.address ? `<span class="wf-mono">${esc(r.address)}</span>` : ''}
        ${gw?.data?.avg != null ? `<span class="wf-dim">gw ${gw.data.avg} ms</span>` : ''}
        <span class="wf-dim">${r.tests.filter((t) => t.ok).length}/${r.tests.length} tests</span>
        ${r.error ? `<span style="color:var(--danger)">${esc(r.error)}</span>` : ''}
        ${r.restore && !r.restore.ok ? `<span style="color:var(--warning)">${esc(r.restore.text)}</span>` : ''}</div>
        ${open ? `${r.link ? `<div class="pr-note">Link: ${esc(r.link.bssid || '')} ${r.link.freq || ''} MHz · tx ${esc(r.link.txRate || '?')} · rx ${esc(r.link.rxRate || '?')} ${r.samples.length > 1 ? `· RSSI over the test ${spark(r.samples.map((x) => x.signal).filter((x) => x != null), 90, 14)}` : ''}</div>` : ''}
          ${r.lease ? `<div class="pr-note">Lease: ${esc(Object.entries(r.lease).filter(([, v]) => v).map(([k, v]) => `${k} ${v}`).join(' · '))}</div>` : ''}
          ${r.restore ? `<div class="pr-note">Afterwards: ${esc(r.restore.text)}; main default route ${esc(r.uplinkBefore || '?')} → ${esc(r.uplinkAfter || '?')}</div>` : ''}
          ${timeline(r.timeline)}
          ${r.wpaLog?.length ? `<details><summary>wpa_supplicant log (${r.wpaLog.length} lines, keys redacted)</summary><div class="pr-log">${esc(r.wpaLog.join('\n'))}</div></details>` : ''}` : ''}
      </div>`;
    }).join('') + '<div class="pr-row"><button class="btn btn-sm" data-act="clearres">Clear results</button></div>' : '<div class="pr-note">No tests yet.</div>';
  }

  // ---------- mount ----------
  function mount(el, st) {
    W.el = el;
    W.st = st;
    W.seq = { radios: -1, scan: -1, results: -1, profiles: -1 };
    el.innerHTML = `
      <div class="wf-blk"><h4>Radios</h4><div class="wf-radios"><div class="pr-note">Loading…</div></div></div>
      <div class="wf-blk"><h4>Scanner</h4><div class="pr-row wf-scanctl"></div><div class="pr-note wf-scanmeta"></div><div class="wf-charts"></div><div class="wf-survey"></div><div class="wf-table" style="margin-top:8px"></div></div>
      <div class="wf-blk"><h4>Client test</h4>
        <div class="pr-note">Associates a radio inside its own network namespace (its address, routes and DNS never touch eth0 or the main routing table), then runs the checks below. The radio is always given back afterwards.</div>
        <div class="wf-form">
          <label>Radio<select name="radio"></select></label>
          <label>Profile<select name="profile"></select></label>
          <label>SSID<input name="ssid" list="wf-ssids" autocomplete="off"><datalist id="wf-ssids"></datalist></label>
          <label>Security<select name="security"><option value="psk">WPA2-PSK</option><option value="psk-sae">WPA2/WPA3 transition</option><option value="sae">WPA3-SAE</option><option value="open">Open</option><option value="owe">OWE (Enhanced Open)</option><option value="eap">Enterprise (PEAP/TTLS)</option></select></label>
          <label class="wf-keyrow">Key / password<input name="psk" type="password" autocomplete="off"></label>
          <label class="chk wf-nmkey"><input type="checkbox" name="useSavedKey"> Use NetworkManager’s saved key</label>
          <label class="wf-eap">EAP method<select name="eapMethod"><option>PEAP</option><option>TTLS</option></select></label>
          <label class="wf-eap">Identity<input name="eapIdentity" autocomplete="off"></label>
          <label>Pin to BSSID (optional)<input name="bssid" placeholder="aa:bb:cc:dd:ee:ff"></label>
          <label>Addressing<select name="ip"><option value="dhcp">DHCP</option><option value="static">Static</option><option value="none">None (associate only)</option></select></label>
          <label class="wf-static">Address/prefix<input name="sAddr" placeholder="192.168.1.50/24"></label>
          <label class="wf-static">Gateway<input name="sGw"></label>
          <label class="wf-static">DNS servers<input name="sDns" placeholder="1.1.1.1"></label>
          <label class="chk"><input type="checkbox" name="pingGateway" checked> Ping the gateway</label>
          <label>Resolve (DNS)<input name="dns" value="example.com"></label>
          <label>Fetch URLs<input name="http" value="https://example.com/"></label>
          <label>Ping hosts<input name="ping" placeholder="1.1.1.1, 8.8.8.8"></label>
          <label>Pings per target<input name="count" type="number" min="1" max="50" value="5"></label>
          <label>Stay connected afterwards (s)<input name="holdS" type="number" min="0" max="600" value="0"></label>
          <label>iperf3 server[:port] (optional)<input name="iperf" placeholder="10.0.0.2"></label>
          <label>iperf3 seconds<input name="iperfS" type="number" min="1" max="60" value="5"></label>
          <label class="chk"><input type="checkbox" name="iperfR"> iperf3 download (-R)</label>
          <label>Save as profile (name)<input name="saveAs" placeholder="optional"></label>
        </div>
        <div class="pr-note wf-warn" style="margin-top:6px"></div>
        <div class="pr-row"><button class="btn btn-primary" data-act="run">Run test</button><button class="btn btn-sm" data-act="delprofile">Delete selected profile</button></div>
        <div class="wf-running"></div>
        <h4 style="margin-top:12px">Recent results</h4><div class="wf-results"></div>
      </div>`;
    el.addEventListener('click', onClick);
    el.addEventListener('change', onChange);
    el.addEventListener('input', (e) => { if (e.target.name === 'ssid' || e.target.name === 'security') formVis(); });
    update(st);
  }

  async function onClick(e) {
    const th = e.target.closest('th[data-sort]');
    if (th) { const k = th.dataset.sort; W.sort = { key: k, dir: W.sort.key === k ? -W.sort.dir : k === 'ssid' || k === 'security' ? 1 : -1 }; renderTable(); return; }
    const tr = e.target.closest('tr.wf-r');
    if (tr && !e.target.closest('button')) { const b = tr.dataset.bssid; W.open.has(b) ? W.open.delete(b) : W.open.add(b); renderTable(); return; }
    const a = e.target.closest('[data-act]');
    if (!a) return;
    const act = a.dataset.act;
    try {
      if (act === 'scan') { a.disabled = true; a.textContent = 'Scanning…'; await papi('POST', '/api/wifi/scan', { radio: $('.wf-scanradio')?.value }); }
      else if (act === 'clearscan') await papi('DELETE', '/api/wifi/scan');
      else if (act === 'chans') { W.showChans.has(a.dataset.phy) ? W.showChans.delete(a.dataset.phy) : W.showChans.add(a.dataset.phy); renderRadios(); }
      else if (act === 'managed') { await papi('POST', `/api/wifi/radios/${a.dataset.phy}/managed`, { managed: a.dataset.v === '1' }); load('radios'); }
      else if (act === 'reg') {
        const c = $('.wf-reg').value.trim();
        const r = await papi('POST', '/api/wifi/reg', { country: c });
        $('.wf-regnote').textContent = r.note || `Regulatory domain is now ${r.country}.`;
        load('radios');
      } else if (act === 'run') {
        a.disabled = true;
        const o = readForm();
        await papi('POST', '/api/wifi/test', o).catch((err) => { a.disabled = false; throw err; });
        $('.wf-form [name=psk]').value = '';
        $('.wf-form [name=saveAs]').value = '';
        if (o.saveAs) load('profiles');
      } else if (act === 'cancel') await papi('POST', `/api/wifi/test/${a.dataset.id}/cancel`);
      else if (act === 'res') { const id = a.dataset.id; W.openRes.has(id) ? W.openRes.delete(id) : W.openRes.add(id); renderResults(); }
      else if (act === 'clearres') await papi('DELETE', '/api/wifi/results');
      else if (act === 'delprofile') { const id = $('.wf-form [name=profile]').value; if (id) { await papi('DELETE', `/api/wifi/profiles/${id}`); load('profiles'); } }
      else if (act === 'usebss') {
        const b = W.scan.bss.find((x) => x.bssid === a.dataset.bssid);
        const f = $('.wf-form');
        f.querySelector('[name=ssid]').value = b.ssid;
        if (b.securityKind !== 'wep') f.querySelector('[name=security]').value = b.securityKind;
        formVis();
        f.scrollIntoView({ behavior: 'smooth', block: 'center' });
      }
    } catch {}
  }

  async function onChange(e) {
    const t = e.target;
    if (t.classList.contains('wf-group')) { W.group = t.checked; renderTable(); }
    else if (t.classList.contains('wf-band')) { W.band = t.value; renderCharts(); renderTable(); }
    else if (t.classList.contains('wf-auto') || t.classList.contains('wf-autoint')) {
      try { await papi('PUT', '/api/wifi/autoscan', { enabled: $('.wf-auto').checked, intervalS: +$('.wf-autoint').value, radio: $('.wf-scanradio').value }); } catch {}
    } else if (t.name === 'profile') loadProfile(t.value);
    else if (t.name === 'ssid') {
      // Pick the security the scan saw for this SSID (strongest BSS).
      const b = (W.scan?.bss || []).filter((x) => x.ssid === t.value).sort((x, y) => (y.signal ?? -200) - (x.signal ?? -200))[0];
      if (b && b.securityKind !== 'wep') $('.wf-form [name=security]').value = b.securityKind;
      formVis();
    } else if (t.name === 'security' || t.name === 'ip' || t.name === 'radio') formVis();
  }

  function update(st) {
    W.st = st;
    if (!W.el) return;
    if (st.radiosSeq !== W.seq.radios) { W.seq.radios = st.radiosSeq; load('radios'); }
    if (st.scan.seq !== W.seq.scan) { W.seq.scan = st.scan.seq; load('scan'); }
    else if (W.scan) renderScanMeta(); // "Scanning…" state and relative times
    if (st.resultsSeq !== W.seq.results) { W.seq.results = st.resultsSeq; load('results'); }
    if (st.profilesCount !== W.seq.profiles) { W.seq.profiles = st.profilesCount; load('profiles'); }
    renderRunning();
  }

  function unmount() { W.el = null; }

  periphRegister('wifi', { mount, update, unmount });
})();
