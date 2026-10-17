// Bluetooth LE scanner panel. Device list arrives over /ws/ble (~1/s while it changes);
// the section's status (controllers, scan state) comes from the regular status stream.
(() => {
  let el, ws, st = {}, snap = null;
  const open = new Set(); // expanded device rows
  const prefs = Object.assign({ live: false, q: '', minRssi: '', beacons: false, sort: 'rssi' }, JSON.parse(localStorage.getItem('ble-prefs') || '{}'));
  const save = () => localStorage.setItem('ble-prefs', JSON.stringify(prefs));

  // RSSI sparkline: fixed −100…−30 dBm scale so devices can be compared at a glance.
  function rssiSpark(h, w = 60, ht = 16) {
    if (!h?.length) return '';
    const n = 60, x = i => ((n - h.length + i) / (n - 1)) * w, y = v => ht - Math.max(0, Math.min(1, (v + 100) / 70)) * ht;
    const pts = h.map((v, i) => `${x(i).toFixed(1)},${y(v).toFixed(1)}`).join(' ');
    return `<svg width="${w}" height="${ht}" viewBox="0 0 ${w} ${ht}" style="display:block"><polyline points="${pts}" fill="none" stroke="#6cb4ee" stroke-width="1.2"/></svg>`;
  }
  const rssiCls = r => r == null ? '' : r >= -60 ? 'on' : r >= -80 ? 'warn' : 'bad';

  function mount(e, s) {
    el = e; st = s || {};
    el.innerHTML = `
      <div class="pr-row" data-k="ctl"></div>
      <div class="pr-row">
        <button class="btn btn-sm btn-primary" data-a="scan10">Scan 10 s</button>
        <button class="btn btn-sm" data-a="scan60">Scan 60 s</button>
        <label><input type="checkbox" data-k="live" ${prefs.live ? 'checked' : ''}> Live while open</label>
        <button class="btn btn-sm btn-danger" data-a="stop">Stop</button>
        <button class="btn btn-sm" data-a="clear">Clear list</button>
      </div>
      <div class="pr-row pr-note" data-k="state"></div>
      <div class="pr-row">
        <input type="search" data-k="q" placeholder="Filter name, address, type" value="${esc(prefs.q)}" style="flex:1;min-width:140px">
        <select data-k="minRssi">${['', -60, -70, -80, -90].map(v => `<option value="${v}" ${String(prefs.minRssi) === String(v) ? 'selected' : ''}>${v === '' ? 'Any signal' : '≥ ' + v + ' dBm'}</option>`).join('')}</select>
        <select data-k="sort">${[['rssi', 'Strongest'], ['seen', 'Last seen'], ['first', 'Newest'], ['name', 'Name']].map(([v, l]) => `<option value="${v}" ${prefs.sort === v ? 'selected' : ''}>${l}</option>`).join('')}</select>
        <label><input type="checkbox" data-k="beacons" ${prefs.beacons ? 'checked' : ''}> Beacons/sensors only</label>
      </div>
      <div data-k="list"></div>`;
    el.addEventListener('click', onClick);
    el.addEventListener('input', onInput);
    el.addEventListener('change', onInput);
    ws = pws('/ws/ble', {
      onopen: () => ws.send({ live: prefs.live }),
      onmessage: m => { if (m?.type === 'devices') { snap = m; renderList(); } else if (m?.type === 'error') toast(m.message, 'error'); },
    });
    update(st);
  }

  async function onClick(e) {
    const a = e.target.closest('[data-a]')?.dataset.a;
    const row = e.target.closest('[data-addr]');
    if (a === 'scan10' || a === 'scan60') await papi('POST', '/api/ble/scan', { duration: a === 'scan10' ? 10 : 60, controller: el.querySelector('[data-k=ctrl]')?.value || undefined }).catch(() => {});
    else if (a === 'stop') {
      if (prefs.live) { prefs.live = false; save(); el.querySelector('[data-k=live]').checked = false; ws.send({ live: false }); }
      await papi('POST', '/api/ble/stop').catch(() => {});
    } else if (a === 'clear') await papi('DELETE', '/api/ble/devices').catch(() => {});
    else if (row && !e.target.closest('.ble-detail')) {
      const k = row.dataset.addr;
      open.has(k) ? open.delete(k) : open.add(k);
      renderList();
    }
  }

  function onInput(e) {
    const k = e.target.dataset.k;
    if (!k) return;
    if (k === 'live') { prefs.live = e.target.checked; ws.send({ live: prefs.live, controller: el.querySelector('[data-k=ctrl]')?.value || undefined }); }
    else if (k === 'ctrl') { if (e.type === 'change') papi('POST', '/api/ble/settings', { controller: e.target.value }).catch(() => {}); return; }
    else if (k === 'restore') { if (e.type === 'change') papi('POST', '/api/ble/settings', { restoreRadio: e.target.checked }).catch(() => {}); return; }
    else prefs[k] = e.target.type === 'checkbox' ? e.target.checked : e.target.value;
    save();
    renderList();
  }

  function update(s) {
    st = s || {};
    if (!el) return;
    const ctl = el.querySelector('[data-k=ctl]');
    const cs = st.controllers || [];
    const sig = JSON.stringify([cs, st.defaultController, st.restoreRadio]);
    if (ctl.dataset.sig !== sig) {
      ctl.dataset.sig = sig;
      const def = cs.find(c => c.address && c.address === st.defaultController) || cs.find(c => c.name === st.defaultController) || cs[0];
      ctl.innerHTML = !cs.length ? '<span class="pr-pill bad">No Bluetooth controller</span>' : `
        <label>Controller <select data-k="ctrl">${cs.map(c => `<option value="${esc(c.name)}" ${c === def ? 'selected' : ''}>${esc(c.name)} · ${esc(c.bus)}</option>`).join('')}</select></label>
        ${cs.map(c => `<span class="pr-note">${esc(c.name)} ${esc(c.address || '')} <span class="pr-pill ${c.powered ? 'on' : c.rfkill?.soft || c.rfkill?.hard ? 'warn' : ''}">${c.rfkill?.hard ? 'hard-blocked' : c.rfkill?.soft ? 'rfkill blocked' : c.powered ? 'powered' : esc(c.powerState || 'off')}</span></span>`).join(' ')}
        <label><input type="checkbox" data-k="restore" ${st.restoreRadio !== false ? 'checked' : ''}> Put radio back to off/blocked after scanning</label>`;
    }
    const sc = st.scan;
    let h = sc ? `<span class="pr-pill on">${sc.stopping ? 'Stopping' : 'Scanning'} ${esc(sc.controller)}</span> ${sc.live ? 'live while this page is open' : sc.until ? Math.max(0, Math.round((sc.until - Date.now()) / 1000)) + ' s left' : ''} · ${sc.packets} adverts`
      : `<span class="pr-pill">Idle</span>${st.lastScan ? ` last scan ${fmtAgo(st.lastScan.ended)} on ${esc(st.lastScan.ctrl)}, ${st.lastScan.packets} adverts` : ''}`;
    h += ` · ${st.recent || 0} heard in the last 5 min`;
    if (st.error) h += ` <span class="pr-pill bad">${esc(st.error)}</span>`;
    el.querySelector('[data-k=state]').innerHTML = h;
  }

  function matches(d) {
    if (prefs.minRssi !== '' && (d.rssi == null || d.rssi < +prefs.minRssi)) return false;
    if (prefs.beacons && !d.beacons.length) return false;
    const q = prefs.q.trim().toLowerCase();
    return !q || [d.name, d.addr, d.addrType, d.kind].some(v => (v || '').toLowerCase().includes(q));
  }

  function renderList() {
    if (!el || !snap) return;
    const sorters = { rssi: (a, b) => (b.rssi ?? -999) - (a.rssi ?? -999), seen: (a, b) => b.lastSeen - a.lastSeen, first: (a, b) => b.firstSeen - a.firstSeen, name: (a, b) => (a.name || '￿').localeCompare(b.name || '￿') };
    const all = snap.devices;
    const list = all.filter(matches).sort(sorters[prefs.sort] || sorters.rssi);
    const host = el.querySelector('[data-k=list]');
    if (!all.length) { host.innerHTML = `<div class="pr-note">No advertisers yet. Start a scan.</div>`; return; }
    let h = `<div class="pr-note">${list.length} of ${all.length} shown · tap a row for details</div><table class="pr-tbl"><thead><tr><th>Signal</th><th>Device</th><th>What it is</th></tr></thead><tbody>`;
    for (const d of list) {
      h += `<tr data-addr="${esc(d.addr)}" style="cursor:pointer">
        <td style="white-space:nowrap"><span class="pr-pill ${rssiCls(d.rssi)}">${d.rssi ?? '–'} dBm</span>${rssiSpark(d.rssiHist)}</td>
        <td style="word-break:break-word"><b>${esc(d.name || '(no name)')}</b><br><span class="pr-endpoint">${esc(d.addr)}</span> <span class="pr-note">${esc(d.addrType)}${d.connectable ? ' · connectable' : ''} · ${fmtAgo(d.lastSeen)}</span></td>
        <td style="word-break:break-word">${esc(d.kind || '')}</td></tr>`;
      if (open.has(d.addr)) h += `<tr class="ble-detail"><td colspan="3">${detail(d)}</td></tr>`;
    }
    host.innerHTML = h + '</tbody></table>';
  }

  function kv(k, v) { return `<div><span class="pr-note">${esc(k)}:</span> ${v}</div>`; }
  function detail(d) {
    let h = '<div class="pr-card" style="margin-top:0">';
    h += kv('First / last seen', `${esc(new Date(d.firstSeen).toLocaleTimeString())} / ${esc(new Date(d.lastSeen).toLocaleTimeString())} · ${d.count} packets via ${esc(d.controller)}`);
    h += kv('PDU', `${esc(d.lastPdu)}${d.connectable ? ' · connectable' : ' · not connectable'}${d.scannable ? ' · scannable' : ''}`);
    if (d.txPower != null) h += kv('TX power', `${d.txPower} dBm${d.rssi != null ? ` (path loss ${d.txPower - d.rssi} dB)` : ''}`);
    if (d.flags != null) h += kv('Flags', fmtHex(d.flags) + [[1, 'LE limited'], [2, 'LE general'], [4, 'no BR/EDR']].filter(([b]) => d.flags & b).map(([, l]) => ' · ' + l).join(''));
    if (d.appearance != null) h += kv('Appearance', fmtHex(d.appearance, 4));
    for (const b of d.beacons) {
      h += kv(b.kind, `<b>${esc(b.label)}</b>`);
      if (b.fields) h += `<div class="pr-note" style="margin-left:12px">${Object.entries(b.fields).map(([k, v]) => `${esc(k)} = ${esc(typeof v === 'object' ? JSON.stringify(v) : v)}`).join(' · ')}</div>`;
    }
    for (const m of d.mfr) h += kv('Manufacturer', `${esc(m.companyName || 'unknown')} ${fmtHex(m.company, 4)} <span class="pr-endpoint" style="word-break:break-all">${esc(m.data)}</span>`);
    if (d.uuids.length) h += kv('Services', d.uuids.map(u => esc(u.uuid) + (u.name ? ` (${esc(u.name)})` : '')).join(', '));
    for (const s of d.svc) h += kv('Service data', `${esc(s.uuid)}${s.name ? ` (${esc(s.name)})` : ''} <span class="pr-endpoint" style="word-break:break-all">${esc(s.data)}</span>`);
    if (d.adv) h += kv('Raw adv', `<span class="pr-endpoint" style="word-break:break-all">${esc(d.adv)}</span>`);
    if (d.scanRsp) h += kv('Raw scan rsp', `<span class="pr-endpoint" style="word-break:break-all">${esc(d.scanRsp)}</span>`);
    return h + '</div>';
  }

  periphRegister('ble', {
    mount,
    update,
    unmount() { ws?.close(); ws = null; el = null; snap = null; },
  });
})();
