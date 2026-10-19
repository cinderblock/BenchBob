// I²C panel: per bus an address map and device cards (identification, live readings with
// sparklines, Measure / auto-poll controls), plus a raw-tools dialog. All clicks go through
// one delegated handler on the section, so re-rendering on every status push is safe.
(() => {
  let root = null, last = null;
  const mapOpen = new Set();
  const enc = encodeURIComponent;

  function spark(arr, w = 90, h = 22) {
    if (!arr || arr.length < 2) return '';
    const lo = Math.min(...arr), hi = Math.max(...arr), span = hi - lo || 1;
    const pts = arr.map((v, i) => `${(i / (arr.length - 1) * w).toFixed(1)},${(h - 2 - (v - lo) / span * (h - 4)).toFixed(1)}`).join(' ');
    return `<svg width="${w}" height="${h}" viewBox="0 0 ${w} ${h}" style="display:block"><polyline points="${pts}" fill="none" stroke="var(--accent)" stroke-width="1.5"/></svg>`;
  }

  function fmtVal(v) {
    if (v.v == null) return '—';
    if (typeof v.v === 'number') return (v.d != null ? v.v.toFixed(v.d) : String(v.v)) + (v.unit ? ' ' + v.unit : '');
    if (typeof v.v === 'boolean') return v.v ? 'yes' : 'no';
    return String(v.v) + (v.unit ? ' ' + v.unit : '');
  }

  const confPill = c => c === 'id' ? '<span class="pr-pill on">ID matched</span>' : c === 'likely' ? '<span class="pr-pill">likely</span>' : c === 'guess' ? '<span class="pr-pill warn">guess</span>' : '';

  function addrMap(b) {
    const at = new Map(b.devices.map(d => [d.addr, d]));
    let h = '<div style="overflow-x:auto"><table class="pr-tbl" style="font-family:\'SF Mono\',ui-monospace,monospace;font-size:.7rem;width:auto"><tr><th></th>';
    for (let c = 0; c < 16; c++) h += `<th style="padding:2px 4px">${c.toString(16)}</th>`;
    h += '</tr>';
    for (let r = 0; r < 8; r++) {
      h += `<tr><th style="padding:2px 4px">${r}0</th>`;
      for (let c = 0; c < 16; c++) {
        const a = r * 16 + c;
        const d = at.get(a);
        const out = a < 0x08 || a > 0x77;
        const txt = out ? '' : d ? (d.claimed ? 'UU' : a.toString(16).padStart(2, '0')) : '--';
        const style = d ? (d.claimed ? 'color:var(--warning)' : 'color:var(--success);font-weight:700') : 'color:var(--dim)';
        h += `<td style="padding:2px 4px;${style}">${txt}</td>`;
      }
      h += '</tr>';
    }
    return h + '</table></div>';
  }

  function devCard(b, d) {
    const id = `${b.bus}:${d.addr}`;
    const title = d.claimed ? `Claimed by kernel driver <b>${esc(d.claimed)}</b>` : esc(d.name || 'Unknown device');
    let h = `<div class="pr-card"><h4>${title}<small>${d.hex}${d.what ? ' · ' + esc(d.what) : ''}${d.label ? ' · ' + esc(d.label) : ''}</small>${confPill(d.confidence)}${d.readout?.stale ? '<span class="pr-pill warn">not fresh</span>' : ''}</h4>`;
    if (d.note) h += `<div class="pr-note">${esc(d.note)}</div>`;
    if (d.hint) h += `<div class="pr-note" style="color:var(--warning)">${esc(d.hint)}</div>`;
    if (!d.name && !d.claimed && d.hints.length) h += `<div class="pr-note">Commonly at this address: ${esc(d.hints.join(', '))}</div>`;
    const r = d.readout;
    if (r?.values?.length) {
      h += '<div style="display:flex;flex-wrap:wrap;gap:6px 18px;margin-top:6px">';
      for (const v of r.values) {
        const hist = d.hist[v.k];
        h += `<div style="min-width:110px"><div class="spark-lbl">${esc(v.label)}</div><div class="spark-val" style="${r.stale ? 'color:var(--dim)' : ''}">${esc(fmtVal(v))}</div>${hist && hist.length > 1 ? spark(hist) : ''}</div>`;
      }
      h += '</div>';
    }
    if (r?.pins) {
      h += '<div style="display:flex;flex-wrap:wrap;gap:4px;margin-top:6px">' + r.pins.map(p => `<span class="pr-pill${p.level ? ' on' : ''}" style="font-family:monospace">${esc(p.name)} ${p.level}${p.dir ? ' ' + p.dir : ''}${p.pull ? ' ↑' : ''}</span>`).join('') + '</div>';
    }
    if (r?.channels) {
      h += '<div style="display:grid;grid-template-columns:repeat(auto-fill,minmax(92px,1fr));gap:4px;margin-top:6px">' + r.channels.map(c => `<div class="pr-note" style="background:var(--surface3);border-radius:4px;padding:2px 6px">CH${c.ch} <b style="color:var(--text)">${c.full ? 'full ' + c.full : c.duty + '%'}</b><div style="height:3px;background:var(--surface);border-radius:2px"><div style="height:3px;width:${c.duty}%;background:var(--accent);border-radius:2px"></div></div></div>`).join('') + '</div>';
    }
    if (r?.note) h += `<div class="pr-note" style="margin-top:4px">${esc(r.note)}</div>`;
    if (d.error) h += `<div class="pr-note" style="color:var(--danger)">${esc(d.error)}</div>`;
    if (d.readAt) h += `<div class="pr-note">Read ${fmtAgo(d.readAt)}</div>`;
    h += '<div class="pr-row">';
    if (d.canRead && !d.claimed) h += `<button class="btn btn-sm" data-act="read" data-id="${id}">Read</button>`;
    if (d.canMeasure && !d.claimed) h += `<button class="btn btn-sm btn-primary" data-act="measure" data-id="${id}">Measure</button>`;
    if (d.pollable && !d.claimed) h += `<label><input type="checkbox" data-act="poll" data-id="${id}" ${d.poll ? 'checked' : ''}> auto-read every ${d.interval / 1000} s</label>`;
    if (d.canMeasure && !d.claimed) h += `<label><input type="checkbox" data-act="auto" data-id="${id}" ${d.auto ? 'checked' : ''}> auto-measure (writes)</label>`;
    if (d.chip === 'at24') h += `<button class="btn btn-sm" data-act="eeprom" data-id="${id}">EEPROM contents</button>`;
    h += `<button class="btn btn-sm" data-act="dsettings" data-id="${id}">Settings</button>`;
    h += `<button class="btn btn-sm" data-act="raw" data-id="${id}">Raw tools</button></div>`;
    if (d.canMeasure && d.measureNote && !d.claimed) h += `<div class="pr-note">Measure ${esc(d.measureNote)}.</div>`;
    return h + '</div>';
  }

  function render(st) {
    last = st;
    if (!root) return;
    if (!st.buses.length) { root.innerHTML = `<div class="empty">No user I²C buses. Turn on I²C1 (header pins 3/5) or an extra bus, or plug in a USB adapter.</div><div class="pr-row" style="justify-content:center">${window.gpioIfaceBtn?.('i2c', 'Bus speed &amp; more buses…') || ''}</div>`; return; }
    let h = '';
    for (const b of st.buses) {
      const open = mapOpen.has(b.bus);
      h += `<div class="pr-card" style="background:transparent">
        <h4>i2c-${b.bus}<small>${esc(b.label)}${b.hz ? ` · ${b.hz / 1000} kHz` : ''} · ${esc(b.funcs.join(', ') || 'no access')}</small></h4>
        <div class="pr-note">${b.scanning ? 'Scanning…' : b.lastScan ? `Scanned ${fmtAgo(b.lastScan)} (${b.scanMs} ms) · ${b.devices.length} device${b.devices.length === 1 ? '' : 's'} · ${b.autoScan ? `rescans every ${b.autoScan} s` : 'rescans on demand only'}` : 'Not scanned yet'}${b.plainI2c ? '' : ' · SMBus only (no combined transfers)'}</div>
        ${b.error ? `<div class="pr-note" style="color:var(--danger)">${esc(b.error)}</div>` : ''}
        <div class="pr-row">
          <button class="btn btn-sm btn-primary" data-act="scan" data-bus="${b.bus}" ${b.scanning ? 'disabled' : ''}>Scan</button>
          <button class="btn btn-sm" data-act="deep" data-bus="${b.bus}" ${b.scanning ? 'disabled' : ''}>Deep identify</button>
          <button class="btn btn-sm" data-act="map" data-bus="${b.bus}">${open ? 'Hide' : 'Show'} address map</button>
          <button class="btn btn-sm" data-act="bsettings" data-bus="${b.bus}">Bus settings</button>
          <button class="btn btn-sm" data-act="rawbus" data-bus="${b.bus}">Raw tools</button>
        </div>
        ${open ? addrMap(b) : ''}
        ${b.devices.length ? b.devices.map(d => devCard(b, d)).join('') : `<div class="pr-note">Nothing answered. Wire SDA${b.bus === 1 ? ' to pin 3, SCL to pin 5' : ', SCL'}, 3V3 and GND; most breakouts have their own pull-ups.</div>`}
      </div>`;
    }
    const ib = window.gpioIfaceBtn?.('i2c', 'Bus speed &amp; more buses…');
    if (ib) h += `<div class="pr-row" style="margin-top:6px">${ib}<span class="pr-note">I²C1 speed, extra controllers (I²C3–6) and a software bus: config.txt, needs a reboot</span></div>`;
    if (st.hiddenBuses?.length) h += `<div class="pr-note" style="margin-top:6px">Not shown: ${esc(st.hiddenBuses.join(', '))} (HDMI display EDID buses).</div>`;
    root.innerHTML = h;
  }

  function find(id) {
    const [bus, addr] = id.split(':').map(Number);
    const b = last?.buses.find(x => x.bus === bus);
    return { bus, addr, b, d: b?.devices.find(x => x.addr === addr) };
  }

  // ---------- dialogs ----------

  function hexdump(bytes, start = 0) {
    let out = '';
    for (let i = 0; i < bytes.length; i += 16) {
      const row = bytes.slice(i, i + 16);
      out += (start + i).toString(16).padStart(4, '0') + '  ' + row.map(b => b == null ? '--' : b.toString(16).padStart(2, '0')).join(' ').padEnd(48) + '  ' + row.map(b => b == null ? ' ' : b >= 32 && b < 127 ? String.fromCharCode(b) : '.').join('') + '\n';
    }
    return out;
  }

  function devSettings(id) {
    const { bus, addr, d } = find(id);
    if (!d) return;
    const opts = d.opts.map(o => `<label>${esc(o.label)} ${o.type === 'select'
      ? `<select data-opt="${esc(o.key)}">${o.choices.map(c => `<option value="${c.v}" ${c.v == o.value ? 'selected' : ''}>${esc(c.label)}</option>`).join('')}</select>`
      : `<input data-opt="${esc(o.key)}" type="number" step="any" value="${esc(o.value)}" style="width:90px"> ${esc(o.unit || '')}`}</label>`).join('');
    showModal(`<h3>${esc(d.name || 'Device')} at ${d.hex} on i2c-${bus}</h3>
      <div class="pr-row"><label>Name <input id="i2c-lbl" value="${esc(d.label)}" placeholder="e.g. Bench temperature"></label></div>
      <div class="pr-row"><label>Poll / measure interval <select id="i2c-iv">${[500, 1000, 2000, 5000, 10000, 30000, 60000].map(v => `<option value="${v}" ${v === d.interval ? 'selected' : ''}>${v < 1000 ? v + ' ms' : v / 1000 + ' s'}</option>`).join('')}</select></label></div>
      ${opts ? `<div class="pr-row">${opts}</div>` : ''}
      <p class="pr-note">Saved for this address on this bus, so it survives restarts and re-plugging.</p>
      <div class="modal-btns"><button class="btn" id="mc">Cancel</button><button class="btn btn-primary" id="mk">Save</button></div>`);
    Q('#mc').onclick = hideModal;
    Q('#mk').onclick = async () => {
      const o = {};
      Q('#modal').querySelectorAll('[data-opt]').forEach(i => o[i.dataset.opt] = Number(i.value));
      try { await papi('PUT', `/api/i2c/${bus}/${addr}/settings`, { label: Q('#i2c-lbl').value, interval: Number(Q('#i2c-iv').value), ...(d.opts.length ? { opts: o } : {}) }); hideModal(); } catch {}
    };
  }

  function busSettings(bus) {
    const b = last?.buses.find(x => x.bus === bus);
    if (!b) return;
    showModal(`<h3>i2c-${bus} settings</h3><p class="pr-note">Background rescans use i2cdetect's safe probes (quick write, or read-byte at 0x30–0x37 and 0x50–0x5F) and pick up boards plugged in while running. Addresses owned by kernel drivers are never probed.</p>
      <div class="pr-row"><label>Rescan every <select id="i2c-as">${[0, 10, 30, 60, 300].map(v => `<option value="${v}" ${v === b.autoScan ? 'selected' : ''}>${v ? v + ' s' : 'never (manual only)'}</option>`).join('')}</select></label></div>
      <div class="modal-btns"><button class="btn" id="mc">Cancel</button><button class="btn btn-primary" id="mk">Save</button></div>`);
    Q('#mc').onclick = hideModal;
    Q('#mk').onclick = async () => { try { await papi('PUT', `/api/i2c/${bus}/settings`, { autoScan: Number(Q('#i2c-as').value) }); hideModal(); } catch {} };
  }

  async function eepromDlg(id) {
    const { bus, addr, d } = find(id);
    const size = d?.opts.find(o => o.key === 'size')?.value || 256;
    showModal(`<h3>EEPROM at ${fmtHex(addr)} on i2c-${bus}</h3><p class="pr-note">Size setting: ${size} bytes (change it in Settings if this is wrong; a 24C32+ needs 2-byte addressing). Showing up to 4 KiB.</p><div class="pr-log" id="i2c-ee">Reading…</div><div class="modal-btns"><button class="btn btn-primary" id="mk">Close</button></div>`, 'wide');
    Q('#mk').onclick = hideModal;
    try { const r = await papi('GET', `/api/i2c/${bus}/${addr}/eeprom?offset=0&length=${Math.min(size, 4096)}`); Q('#i2c-ee').textContent = hexdump(r.bytes); }
    catch (e) { Q('#i2c-ee').textContent = e.message; }
  }

  const RAW_OPS = [
    ['rbyte', 'Read byte (register)', false], ['rword', 'Read word (register)', false], ['rblock', 'Read block (register, length)', false],
    ['recv', 'Pure read (length, no register)', false], ['dump', 'Dump registers 0x00–0xFF', false],
    ['wbyte', 'Write byte (register, value)', true], ['wword', 'Write word (register, value, MSB first)', true], ['wblock', 'Write block (register, bytes)', true],
    ['xfer', 'Write then read (bytes, length)', true],
  ];

  function rawDlg(bus, addr) {
    const b = last?.buses.find(x => x.bus === bus);
    const d = b?.devices.find(x => x.addr === addr);
    showModal(`<h3>Raw I²C · i2c-${bus}</h3>
      <div class="pr-row"><label>Address <input id="i2c-ra" value="${addr != null ? fmtHex(addr) : '0x'}" style="width:70px"></label>
        <label>Operation <select id="i2c-op">${RAW_OPS.map(([v, l]) => `<option value="${v}">${esc(l)}</option>`).join('')}</select></label></div>
      <div class="pr-row"><label>Register <input id="i2c-rr" value="0x00" style="width:70px"></label>
        <label>Value / bytes <input id="i2c-rv" placeholder="0x27 or 0x00 0x1f" style="width:150px"></label>
        <label>Length <input id="i2c-rl" type="number" min="1" max="256" value="1" style="width:64px"></label></div>
      ${d?.pointerUnsafe ? `<p class="pr-note" style="color:var(--warning)">${d.name ? esc(d.name) + ' treats' : 'An unidentified chip may treat'} register pointers as commands: register reads and dumps can change its state.</p>` : ''}
      <label class="pr-note" style="display:flex;gap:6px;align-items:center" id="i2c-wc-l"><input type="checkbox" id="i2c-wc"> I understand this writes to the device</label>
      <label class="pr-note" style="display:flex;gap:6px;align-items:center"><input type="checkbox" id="i2c-force"> Force (address owned by a kernel driver)</label>
      <div class="pr-log" id="i2c-out" style="margin-top:8px;min-height:3em"></div>
      <div class="modal-btns"><button class="btn" id="mc">Close</button><button class="btn btn-primary" id="mk">Run</button></div>`, 'wide');
    const sync = () => {
      const w = RAW_OPS.find(o => o[0] === Q('#i2c-op').value)[2];
      Q('#i2c-wc-l').style.display = w ? 'flex' : 'none';
      Q('#mk').className = 'btn ' + (w ? 'btn-warn' : 'btn-primary');
      Q('#mk').textContent = w ? 'Write' : 'Run';
    };
    Q('#i2c-op').onchange = sync; sync();
    Q('#mc').onclick = hideModal;
    Q('#mk').onclick = async () => {
      const op = Q('#i2c-op').value, a = Q('#i2c-ra').value.trim(), reg = Q('#i2c-rr').value.trim(), val = Q('#i2c-rv').value.trim();
      const len = Number(Q('#i2c-rl').value) || 1, force = Q('#i2c-force').checked, confirm = Q('#i2c-wc').checked;
      const out = Q('#i2c-out');
      const base = `/api/i2c/${bus}/${enc(a)}`;
      const bytes = val.split(/[\s,]+/).filter(Boolean);
      if (RAW_OPS.find(o => o[0] === op)[2] && !confirm) { out.textContent = 'Tick the confirmation first: this writes to the device.'; return; }
      try {
        let r;
        if (op === 'rbyte') r = await papi('POST', base + '/raw/read', { register: reg, size: 'byte', force });
        else if (op === 'rword') r = await papi('POST', base + '/raw/read', { register: reg, size: 'word', force });
        else if (op === 'rblock') r = await papi('POST', base + '/raw/read', { register: reg, size: 'block', length: Math.min(32, len), force });
        else if (op === 'recv') r = await papi('POST', base + '/raw/read', { size: 'raw', length: len, force });
        else if (op === 'dump') {
          const unsafe = !d || d.pointerUnsafe;
          r = await papi('GET', base + `/dump?${unsafe ? 'confirm=1&' : ''}${force ? 'force=1' : ''}`);
          out.textContent = '      ' + [...Array(16).keys()].map(i => ' ' + i.toString(16)).join(' ') + '\n' + hexdump(r.regs).replace(/^0{2}/gm, '');
          return;
        }
        else if (op === 'wbyte') r = await papi('POST', base + '/raw/write', { register: reg, size: 'byte', value: val, confirm, force });
        else if (op === 'wword') r = await papi('POST', base + '/raw/write', { register: reg, size: 'word', value: val, confirm, force });
        else if (op === 'wblock') r = await papi('POST', base + '/raw/write', { register: reg, size: 'block', bytes, confirm, force });
        else if (op === 'xfer') r = await papi('POST', base + '/transfer', { write: bytes, read: len, confirm, force });
        const all = r.reads ? r.reads.flat() : r.bytes || r.wrote || [];
        out.textContent = (r.wrote ? 'Wrote ' : '') + all.map(x => fmtHex(x)).join(' ') + (r.wordBigEndian != null ? `\nword: ${fmtHex(r.word, 4)} (LE)  ${fmtHex(r.wordBigEndian, 4)} (BE, as most sensors mean it)` : '') + (all.length > 1 && !r.wrote ? '\n' + hexdump(all) : '');
      } catch (e) { out.textContent = e.message; }
    };
  }

  // ---------- events ----------

  async function onClick(e) {
    const t = e.target.closest('[data-act]');
    if (!t || !root?.contains(t)) return;
    const act = t.dataset.act, bus = Number(t.dataset.bus);
    const f = t.dataset.id ? find(t.dataset.id) : null;
    try {
      if (act === 'scan' || act === 'deep') { t.disabled = true; await papi('POST', `/api/i2c/${bus}/scan`, { deep: act === 'deep' }); }
      else if (act === 'map') { mapOpen.has(bus) ? mapOpen.delete(bus) : mapOpen.add(bus); render(last); }
      else if (act === 'bsettings') busSettings(bus);
      else if (act === 'rawbus') rawDlg(bus, null);
      else if (act === 'read' || act === 'measure') { t.disabled = true; await papi('POST', `/api/i2c/${f.bus}/${f.addr}/${act}`); }
      else if (act === 'poll' || act === 'auto') await papi('PUT', `/api/i2c/${f.bus}/${f.addr}/settings`, { [act]: t.checked });
      else if (act === 'dsettings') devSettings(t.dataset.id);
      else if (act === 'eeprom') eepromDlg(t.dataset.id);
      else if (act === 'raw') rawDlg(f.bus, f.addr);
    } catch {} finally { if (t.tagName === 'BUTTON') t.disabled = false; }
  }

  periphRegister('i2c', {
    mount(el, st) { root = el; el.addEventListener('click', onClick); render(st); },
    update(st) { render(st); },
    unmount() { root = null; },
  });
})();
