// 1-Wire panel: masters (search, noise diagnosis), slave cards decoded by family with
// sparklines, I/O switches, EEPROM viewer, iButton touch log, DS2482 bridge binding.
(() => {
  let root = null, last = null;

  function spark(arr, w = 110, h = 24) {
    if (!arr || arr.length < 2) return '';
    const lo = Math.min(...arr), hi = Math.max(...arr), span = hi - lo || 1;
    const pts = arr.map((v, i) => `${(i / (arr.length - 1) * w).toFixed(1)},${(h - 2 - (v - lo) / span * (h - 4)).toFixed(1)}`).join(' ');
    return `<svg width="${w}" height="${h}" viewBox="0 0 ${w} ${h}" style="display:block"><polyline points="${pts}" fill="none" stroke="var(--accent)" stroke-width="1.5"/></svg><div class="pr-note" style="font-size:.65rem">${lo.toFixed(2)} – ${hi.toFixed(2)}</div>`;
  }
  const fmt = v => v.v == null ? '—' : typeof v.v === 'number' ? (v.d != null ? v.v.toFixed(v.d) : String(v.v)) + (v.unit ? ' ' + v.unit : '') : String(v.v) + (v.unit ? ' ' + v.unit : '');

  function slaveCard(s) {
    let h = `<div class="pr-card" style="${s.present ? '' : 'opacity:.55'}"><h4>${esc(s.label || s.name)}<small>${esc(s.label ? s.name + ' · ' : '')}${esc(s.id)} · ${esc(s.desc)}</small>${s.present ? '' : '<span class="pr-pill">gone</span>'}${s.romOk === false ? '<span class="pr-pill bad">ROM CRC bad</span>' : ''}</h4>`;
    const big = s.values.filter(v => v.k !== 'res' && v.k !== 'alarm' && v.k !== 'power' && v.k !== 'addr');
    const small = s.values.filter(v => !big.includes(v));
    if (big.length) {
      h += '<div style="display:flex;flex-wrap:wrap;gap:6px 18px">';
      for (const v of big) h += `<div style="min-width:110px"><div class="spark-lbl">${esc(v.label)}</div><div class="spark-val">${esc(fmt(v))}</div>${spark(s.hist[v.k])}</div>`;
      h += '</div>';
    }
    if (small.length) h += `<div class="pr-note">${small.map(v => `${esc(v.label)}: ${esc(fmt(v))}`).join(' · ')}</div>`;
    if (s.pins) {
      h += '<div style="display:flex;flex-wrap:wrap;gap:4px;margin-top:6px">' + s.pins.map(p => `<button class="pr-pill${p.out ? ' warn' : p.level ? ' on' : ''}" style="border:none;cursor:pointer;font-family:monospace" data-act="pin" data-id="${esc(s.id)}" data-pin="${esc(p.name.replace('PIO-', ''))}" data-on="${p.out ? 0 : 1}">${esc(p.name)}: ${p.level ? 'high' : 'low'}${p.out ? ' · driven low' : ''}</button>`).join('') + '</div><div class="pr-note">Tap a pin to toggle its output transistor (asks first). "Driven low" = transistor on.</div>';
    }
    for (const w of s.warn) h += `<div class="pr-note" style="color:var(--warning)">${esc(w)}</div>`;
    if (s.error) h += `<div class="pr-note" style="color:var(--danger)">${esc(s.error)}${s.module ? ` (kernel module ${esc(s.module)})` : ''}</div>`;
    h += `<div class="pr-row"><span class="pr-note">${s.readAt ? 'Read ' + fmtAgo(s.readAt) : 'Not read yet'} · first seen ${fmtAgo(s.firstSeen)}</span>
      <button class="btn btn-sm" data-act="read" data-id="${esc(s.id)}">Read now</button>
      ${s.eepromSize ? `<button class="btn btn-sm" data-act="eeprom" data-id="${esc(s.id)}">EEPROM (${s.eepromSize} B)</button>` : ''}
      <button class="btn btn-sm" data-act="edit" data-id="${esc(s.id)}">Name${s.resolution ? ' / resolution' : ''}</button></div>`;
    return h + '</div>';
  }

  function render(st) {
    last = st;
    if (!root) return;
    let h = '';
    const ib = window.gpioIfaceBtn?.('w1', '1-Wire pin…') || '';
    if (!st.masters.length) h += `<div class="empty">No 1-Wire master. Turn on the GPIO 1-Wire master or plug in a DS9490R.</div>${ib ? `<div class="pr-row" style="justify-content:center">${ib}</div>` : ''}`;
    for (const m of st.masters) {
      h += `<div class="pr-card" style="background:transparent"><h4>${esc(m.label)}<small>${esc(m.id)} · ${m.slaves} device${m.slaves === 1 ? '' : 's'} · search ${esc(m.searchText)} every ${esc(m.timeout || '?')} s · strong pull-up ${m.pullup ? 'on' : 'off'}${m.bulk ? ' · bulk conversion available' : ''}</small></h4>
        ${m.ghosts ? `<div class="pr-note" style="color:var(--warning)">${m.ghosts} phantom ID${m.ghosts === 1 ? '' : 's'} (family 00) in the last 10 min: the data line is floating. Fit a 4.7 kΩ pull-up from the data pin to 3V3, or ignore this if nothing is connected.</div>` : ''}
        ${m.error ? `<div class="pr-note" style="color:var(--danger)">${esc(m.error)}</div>` : ''}
        <div class="pr-row"><button class="btn btn-sm btn-primary" data-act="search" data-m="${esc(m.id)}">Search now</button>
        <button class="btn btn-sm" data-act="settings">Polling every ${st.interval / 1000} s${st.bulk ? ', simultaneous conversion' : ''}</button>${m.kind === 'gpio' ? ib : ''}</div></div>`;
    }
    if (st.fixture) h += `<div class="pr-note" style="color:var(--warning)">Reading a fixture tree (${esc(st.root)}), not real hardware.</div>`;
    h += st.slaves.map(slaveCard).join('');
    if (st.touchLog.length) {
      h += `<div class="pr-card"><h4>Touch log<small>iButton touches and device arrivals</small></h4><table class="pr-tbl"><tr><th>When</th><th>Device</th><th>Event</th></tr>${st.touchLog.slice(0, 15).map(t => `<tr><td>${new Date(t.t).toLocaleTimeString()} <span class="pr-note">${fmtAgo(t.t)}</span></td><td><code>${esc(t.id)}</code> <span class="pr-note">${esc(t.family)}</span></td><td>${esc(t.event)}</td></tr>`).join('')}</table></div>`;
    }
    if (st.bridges.length) {
      h += `<div class="pr-card"><h4>Bridges</h4>${st.bridges.map(b => `<div class="pr-row"><span>${esc(b.type)} ${b.bus != null ? `i2c-${b.bus} ${esc(b.address)}` : ''}</span><span class="pr-note">${esc(b.note)}</span>${b.type === 'DS2482' && !b.bound && !/claimed/.test(b.note) ? `<button class="btn btn-sm btn-primary" data-act="bind" data-bus="${b.bus}" data-addr="${esc(b.address)}">Bind ds2482 driver</button>` : ''}${b.type === 'DS2482' && b.bound ? `<button class="btn btn-sm" data-act="unbind" data-bus="${b.bus}" data-addr="${esc(b.address)}">Unbind</button>` : ''}</div>`).join('')}</div>`;
    }
    root.innerHTML = h;
  }

  const slave = id => last?.slaves.find(s => s.id === id);

  function hexdump(bytes) {
    let out = '';
    for (let i = 0; i < bytes.length; i += 16) {
      const row = bytes.slice(i, i + 16);
      out += i.toString(16).padStart(4, '0') + '  ' + row.map(b => b.toString(16).padStart(2, '0')).join(' ').padEnd(48) + '  ' + row.map(b => b >= 32 && b < 127 ? String.fromCharCode(b) : '.').join('') + '\n';
    }
    return out;
  }

  function editDlg(id) {
    const s = slave(id);
    if (!s) return;
    showModal(`<h3>${esc(s.name)} <span class="pr-note">${esc(s.id)}</span></h3>
      <div class="pr-row"><label>Name <input id="ow-l" value="${esc(s.label)}" placeholder="e.g. Freezer"></label></div>
      ${s.resolution ? `<div class="pr-row"><label>Resolution <select id="ow-r">${[9, 10, 11, 12].map(b => `<option value="${b}">${b}-bit (${[0.5, 0.25, 0.125, 0.0625][b - 9]} °C, ${[94, 188, 375, 750][b - 9]} ms)</option>`).join('')}</select></label>
        <label><input type="checkbox" id="ow-save"> also save in the sensor's EEPROM</label></div>
        <p class="pr-note">Without saving, the sensor returns to its stored resolution after a power cycle. EEPROM writes are limited (~50k), so don't automate it.</p>` : ''}
      ${s.family === '0x26' ? `<div class="pr-row"><label>Sense resistor <input id="ow-rs" type="number" step="any" placeholder="Ω" style="width:80px"> Ω (for current)</label></div>` : ''}
      <div class="modal-btns"><button class="btn" id="mc">Cancel</button><button class="btn btn-primary" id="mk">Save</button></div>`);
    const cur = s.values.find(v => v.k === 'res');
    if (cur && Q('#ow-r')) Q('#ow-r').value = String(parseInt(cur.v));
    Q('#mc').onclick = hideModal;
    Q('#mk').onclick = async () => {
      try {
        await papi('PUT', `/api/onewire/${s.id}/label`, { label: Q('#ow-l').value, ...(Q('#ow-rs') && Q('#ow-rs').value ? { rsens: Number(Q('#ow-rs').value) } : {}) });
        if (Q('#ow-r') && String(parseInt(cur?.v)) !== Q('#ow-r').value) await papi('PUT', `/api/onewire/${s.id}/resolution`, { bits: Number(Q('#ow-r').value), save: Q('#ow-save').checked });
        hideModal();
      } catch {}
    };
  }

  function settingsDlg() {
    showModal(`<h3>1-Wire polling</h3>
      <div class="pr-row"><label>Read every <select id="ow-iv">${[2000, 5000, 10000, 30000, 60000, 300000].map(v => `<option value="${v}" ${v === last.interval ? 'selected' : ''}>${v / 1000} s</option>`).join('')}</select></label></div>
      <div class="pr-row"><label><input type="checkbox" id="ow-bulk" ${last.bulk ? 'checked' : ''}> Simultaneous conversion when a bus has several temperature sensors (one 750 ms wait instead of one per sensor)</label></div>
      <div class="modal-btns"><button class="btn" id="mc">Cancel</button><button class="btn btn-primary" id="mk">Save</button></div>`);
    Q('#mc').onclick = hideModal;
    Q('#mk').onclick = async () => { try { await papi('PUT', '/api/onewire/settings', { interval: Number(Q('#ow-iv').value), bulk: Q('#ow-bulk').checked }); hideModal(); } catch {} };
  }

  async function onClick(e) {
    const t = e.target.closest('[data-act]');
    if (!t || !root?.contains(t)) return;
    const { act, id } = t.dataset;
    try {
      if (act === 'search') { t.disabled = true; await papi('POST', '/api/onewire/search', { master: t.dataset.m }); toast('Search done', 'success'); }
      else if (act === 'read') { t.disabled = true; await papi('POST', `/api/onewire/${id}/read`); }
      else if (act === 'settings') settingsDlg();
      else if (act === 'edit') editDlg(id);
      else if (act === 'eeprom') {
        showModal(`<h3>EEPROM ${esc(id)}</h3><div class="pr-log" id="ow-ee">Reading…</div><div class="modal-btns"><button class="btn btn-primary" id="mk">Close</button></div>`, 'wide');
        Q('#mk').onclick = hideModal;
        const r = await papi('POST', `/api/onewire/${id}/read?eeprom=1`);
        if (Q('#ow-ee')) Q('#ow-ee').textContent = hexdump(r.eeprom || []);
      }
      else if (act === 'pin') {
        const on = t.dataset.on === '1';
        if (!await confirm2('Switch 1-Wire output', `${on ? 'Turn ON' : 'Turn OFF'} PIO ${t.dataset.pin} of ${id}?`, on ? 'ON = output transistor conducts: the pin is pulled to GND.' : 'OFF = transistor released: the pin floats (or rises with a pull-up).')) return;
        await papi('POST', `/api/onewire/${id}/output`, { pin: t.dataset.pin, on, confirm: true });
      }
      else if (act === 'bind' || act === 'unbind') {
        if (!await confirm2(act === 'bind' ? 'Bind DS2482' : 'Unbind DS2482', act === 'bind' ? 'Load the kernel ds2482 driver for this bridge? It becomes a new 1-Wire master and its I²C address shows as claimed (UU).' : 'Remove the ds2482 master? Devices on it disappear.', `i2c-${t.dataset.bus} ${t.dataset.addr}`)) return;
        await papi('POST', '/api/onewire/bridge', { bus: Number(t.dataset.bus), address: t.dataset.addr, bind: act === 'bind' });
      }
    } catch {} finally { if (t.tagName === 'BUTTON') t.disabled = false; }
  }

  periphRegister('onewire', {
    mount(el, st) { root = el; el.addEventListener('click', onClick); render(st); },
    update(st) { render(st); },
    unmount() { root = null; },
  });
})();
