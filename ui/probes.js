// Debug probes section: one card per probe (OpenOCD GDB server, target config, flash/erase/
// reset/memory actions, live log) and one per MCU in bootloader mode (RP2 UF2, STM32 DFU).
// Cards are built once and then patched in place, so typing in a field isn't clobbered by
// the 2 s status push; only the status block and button states are redrawn.
(() => {
  let root = null;
  const cards = {}; // id -> { el, ws, kind: 'probe'|'boot', since, clientsFor }
  let targets = null;

  const enc = encodeURIComponent;
  const q = (el, s) => el.querySelector(s);
  const pill = (text, cls = '') => `<span class="pr-pill ${cls}">${esc(text)}</span>`;
  const fmtBytes = n => n >= 1048576 ? (n / 1048576).toFixed(1) + ' MB' : n >= 1024 ? (n / 1024).toFixed(1) + ' KB' : n + ' B';

  async function loadTargets() {
    if (targets) return;
    try { targets = await papi('GET', '/api/probes/targets'); } catch { return; }
    const presets = new Set(targets.presets.map(t => t.cfg));
    const dl = document.createElement('datalist');
    dl.id = 'pb-targets';
    dl.innerHTML = targets.presets.map(t => `<option value="${esc(t.cfg)}" label="${esc(t.label)}">`).join('') +
      targets.all.filter(t => !presets.has(t)).map(t => `<option value="${esc(t)}">`).join('');
    root.appendChild(dl);
  }

  // Raw-body upload with query params; resolves the JSON reply or toasts and throws.
  async function upload(url, file) {
    let r, j;
    try { r = await fetch(url, { method: 'POST', body: file }); j = await r.json().catch(() => ({})); }
    catch (e) { toast('Upload failed: ' + e.message, 'error'); throw e; }
    if (!r.ok || j.ok === false) { const m = j.message || r.statusText; toast(m, 'error'); throw new Error(m); }
    return j;
  }

  function serverPill(p) {
    if (p.gdbBuiltIn) return pill('built-in GDB server', 'on');
    if (!p.iface) return pill('unsupported', 'warn');
    const s = p.server;
    if (!s) return pill('idle');
    if (s.state === 'running') return pill(`GDB :${s.ports.gdb}`, 'on');
    if (s.state === 'exited') return s.stopped ? pill('stopped') : pill(`exited (${s.exitCode})`, s.exitCode ? 'bad' : '');
    return pill(s.state, 'warn');
  }

  function jobHtml(d) {
    if (d.job) return `<div class="pr-note">${pill(d.job.kind, 'warn')} ${esc(d.job.stage)}${d.job.pct != null ? ` · ${d.job.pct}%` : ''}…</div>`;
    const j = d.lastJob;
    if (!j) return '';
    return `<div class="pr-note">${pill(j.kind, j.ok ? 'on' : 'bad')} ${esc(j.message || j.stage)} · ${fmtAgo(j.done)}</div>`;
  }

  // ---------- Probe cards ----------

  function probeSkeleton(p) {
    const id = esc(p.id);
    const meta = [p.model && p.model !== p.name ? p.model : '', p.serial ? 'S/N ' + p.serial : '', p.vidpid, p.usb ? 'USB ' + p.usb : ''].filter(Boolean).map(esc).join(' · ');
    let h = `<h4>${esc(p.name)} <span class="pb-pill"></span><small>${meta}${p.sim ? ' · simulated' : ''}</small></h4>`;
    if (p.note) h += `<div class="pr-note">${esc(p.note)}</div>`;
    if (p.gdbBuiltIn) {
      const gdbTty = p.ttys.find(t => t.iface === '00')?.dev || p.ttys[0]?.dev || '';
      const uart = p.ttys.find(t => t.iface === '02')?.dev || '';
      h += `<div class="pr-row"><span class="pr-note">On BenchBob:</span><span class="pr-endpoint">target extended-remote ${esc(gdbTty || '/dev/ttyACM?')}</span></div>
        <div class="pr-note">From another machine: in the Serial panel, start a raw TCP bridge on ${esc(gdbTty || 'the first port')}, then <span class="pr-endpoint">target extended-remote ${esc(location.hostname)}:&lt;bridge port&gt;</span>, followed by <span class="pr-endpoint">monitor swdp_scan</span> and <span class="pr-endpoint">attach 1</span>.${uart ? ` The target's UART is on ${esc(uart)}.` : ''}</div>`;
      return h;
    }
    if (!p.iface) return h;
    if (p.espSerial && p.ttys.length) h += `<div class="pr-note">Serial port: ${p.ttys.map(t => esc(t.dev)).join(', ')} (esptool flashing via the Serial panel).</div>`;
    const tr = p.transports.map(t => `<option value="${t}">${t.toUpperCase()}</option>`).join('');
    h += `<div class="pb-status"></div>
      <div class="pr-row">
        <label>Target <input class="pb-target" list="pb-targets" placeholder="target/….cfg" style="width:15em" spellcheck="false"></label>
        <label>Transport <select class="pb-transport">${tr}</select></label>
        <label>kHz <input class="pb-speed" type="number" min="1" max="100000" style="width:6em"></label>
      </div>
      <div class="pr-row">
        <label>GDB <input class="pb-port" data-k="gdb" type="number" style="width:5.5em"></label>
        <label>Telnet <input class="pb-port" data-k="telnet" type="number" style="width:5.5em"></label>
        <label>TCL <input class="pb-port" data-k="tcl" type="number" style="width:5.5em"></label>
        <button class="btn btn-sm btn-primary pb-start" onclick="pbStart('${id}')">Start GDB server</button>
        <button class="btn btn-sm pb-stop" onclick="pbStop('${id}')">Stop</button>
      </div>
      <div class="pr-note pb-offset"></div>
      <div class="pb-clients"></div>
      <div class="pr-row">
        <button class="btn btn-sm pb-act" onclick="pbAct('${id}','examine')">Detect target</button>
        <button class="btn btn-sm pb-act" onclick="pbAct('${id}','reset',{mode:'run'})">Reset</button>
        <button class="btn btn-sm pb-act" onclick="pbAct('${id}','reset',{mode:'halt'})">Reset + halt</button>
        <button class="btn btn-sm pb-act" onclick="pbAct('${id}','halt')">Halt</button>
        <button class="btn btn-sm pb-act" onclick="pbAct('${id}','resume')">Resume</button>
        <button class="btn btn-sm btn-danger pb-act" onclick="pbErase('${id}')">Mass erase</button>
      </div>
      <div class="pr-row">
        <input class="pb-file" type="file" accept=".elf,.hex,.bin,.srec,.s19,.axf,.out" style="max-width:100%">
        <label>Address <input class="pb-addr" placeholder="0x08000000 (.bin only)" style="width:11em" spellcheck="false"></label>
        <label><input class="pb-verify" type="checkbox" checked> verify</label>
        <label><input class="pb-reset" type="checkbox" checked> reset</label>
        <button class="btn btn-sm btn-primary pb-act" onclick="pbFlash('${id}')">Flash</button>
      </div>
      <div class="pr-row">
        <label>Read memory <input class="pb-maddr" placeholder="0x20000000" style="width:8em" spellcheck="false"></label>
        <label>bytes <input class="pb-mlen" type="number" value="256" min="1" max="65536" style="width:6em"></label>
        <button class="btn btn-sm pb-act" onclick="pbRead('${id}')">Read</button>
      </div>
      <pre class="pr-log pb-out" style="display:none"></pre>
      <div class="pb-job"></div>
      <details class="pb-logbox"><summary>OpenOCD log</summary><pre class="pr-log pb-log"></pre></details>`;
    return h;
  }

  function fillConfig(k, p) {
    const c = p.config;
    const set = (el, v) => { if (el && document.activeElement !== el) el.value = v; };
    set(q(k.el, '.pb-target'), c.target);
    set(q(k.el, '.pb-transport'), c.transport);
    set(q(k.el, '.pb-speed'), c.speed);
    for (const el of k.el.querySelectorAll('.pb-port')) set(el, c.ports[el.dataset.k]);
    const off = q(k.el, '.pb-offset');
    if (off) off.textContent = c.effectivePorts.gdb !== c.ports.gdb ? `This instance adds ${c.effectivePorts.gdb - c.ports.gdb} to every port: GDB ${c.effectivePorts.gdb}, telnet ${c.effectivePorts.telnet}, TCL ${c.effectivePorts.tcl}.` : '';
  }

  function probeStatusHtml(p) {
    const o = p.ocd, bits = [];
    if (o.adapter) bits.push(esc(o.adapter));
    if (o.voltage != null) bits.push(`target ${o.voltage.toFixed(2)} V`);
    if (o.dpidr) bits.push('DPIDR ' + esc(o.dpidr));
    if (o.flash) bits.push('flash ' + esc(o.flash));
    if (p.server?.state === 'running') bits.push(`${o.gdbClients} GDB client${o.gdbClients === 1 ? '' : 's'}`);
    let h = '';
    if (o.cpus.length) h += `<div class="pr-row">${o.cpus.map(c => pill(c, 'on')).join(' ')}</div>`;
    if (bits.length) h += `<div class="pr-note">${bits.join(' · ')}</div>`;
    if (o.errors.length) h += `<div class="pr-note" style="color:var(--danger)">${o.errors.map(esc).join('<br>')}</div>`;
    if (o.hint) h += `<div class="pr-note" style="color:var(--warning)">${esc(o.hint)}</div>`;
    return h;
  }

  async function loadClients(k, id) {
    const box = q(k.el, '.pb-clients');
    try {
      const c = await papi('GET', `/api/probes/${enc(id)}/clients?host=${enc(location.hostname)}&web=${enc(location.origin)}`);
      box.innerHTML = `<div class="pr-row"><span class="pr-note">GDB</span><span class="pr-endpoint">${esc(c.gdb)}</span></div>
        <div class="pr-row"><span class="pr-note">Telnet</span><span class="pr-endpoint">${esc(c.telnet)}</span></div>
        <details><summary>VS Code (cortex-debug) and PlatformIO</summary>
          <div class="pr-note">launch.json configuration:</div><pre class="pr-log">${esc(c.cortexDebug)}</pre>
          <div class="pr-note">platformio.ini (debug through this server; upload through BenchBob's API):</div><pre class="pr-log">${esc(c.platformio)}</pre>
        </details>`;
    } catch { box.innerHTML = ''; }
  }

  function updateProbe(k, p) {
    q(k.el, '.pb-pill').innerHTML = serverPill(p);
    if (p.gdbBuiltIn || !p.iface) return;
    fillConfig(k, p);
    q(k.el, '.pb-status').innerHTML = probeStatusHtml(p);
    q(k.el, '.pb-job').innerHTML = jobHtml(p);
    const s = p.server, live = s && s.state !== 'exited';
    q(k.el, '.pb-start').style.display = live ? 'none' : '';
    q(k.el, '.pb-stop').style.display = live ? '' : 'none';
    for (const el of k.el.querySelectorAll('.pb-target,.pb-transport,.pb-speed,.pb-port')) el.disabled = !!live;
    for (const el of k.el.querySelectorAll('.pb-act')) el.disabled = !!p.job || (s && s.state !== 'running' && s.state !== 'exited');
    const runningSince = s?.state === 'running' ? s.since : 0;
    if (runningSince !== k.clientsFor) {
      k.clientsFor = runningSince;
      if (runningSince) loadClients(k, p.id); else q(k.el, '.pb-clients').innerHTML = '';
    }
  }

  // ---------- Bootloader cards ----------

  function bootSkeleton(b) {
    const id = esc(b.id);
    const meta = [b.serial ? 'S/N ' + b.serial : '', b.vidpid, b.usb ? 'USB ' + b.usb : ''].filter(Boolean).map(esc).join(' · ');
    let h = `<h4>${esc(b.name)} ${pill('bootloader', 'warn')}<small>${meta}${b.sim ? ' · simulated' : ''}</small></h4><div class="pb-status"></div>`;
    if (b.kind === 'rp2-bootsel') {
      h += `<div class="pr-row"><input class="pb-file" type="file" accept=".uf2" style="max-width:100%">
        <button class="btn btn-sm btn-primary pb-act" onclick="pbUf2('${id}')">Flash UF2</button></div>`;
    } else if (b.kind === 'stm32-dfu') {
      h += `<div class="pr-row pb-dfu"><input class="pb-file" type="file" accept=".bin" style="max-width:100%">
        <label>Address <input class="pb-addr" value="0x08000000" style="width:8em" spellcheck="false"></label>
        <button class="btn btn-sm btn-primary pb-act" onclick="pbDfu('${id}')">Flash .bin</button></div>`;
    }
    h += `<div class="pb-job"></div><details class="pb-logbox"><summary>Log</summary><pre class="pr-log pb-log"></pre></details>`;
    return h;
  }

  function updateBoot(k, b) {
    let h = '';
    if (b.kind === 'rp2-bootsel') {
      if (b.mount) h += `<div class="pr-note">Drive ${esc(b.label)} at ${esc(b.mount)}${b.readOnly ? ' (mounted read-only; flashing remounts it read-write)' : ''}</div>`;
      else h += `<div class="pr-note">${b.block ? `Drive ${esc(b.block)} isn't mounted yet; flashing mounts it.` : 'Waiting for its USB drive…'}</div>`;
      if (b.info) h += `<div class="pr-note">${[b.info.bootloader, ...Object.entries(b.info.fields).map(([k, v]) => `${k}: ${v}`)].map(esc).join(' · ')}</div>`;
      h += `<div class="pr-note">${b.tool.path ? 'picotool is installed.' : `picotool isn't installed (apt package ${esc(b.tool.pkg)}); UF2 copy works without it.`}</div>`;
    } else if (b.kind === 'stm32-dfu') {
      h += b.tool.path ? '<div class="pr-note">Flashing uses dfu-util, alt setting 0 (internal flash), and starts the app afterwards.</div>'
        : `<div class="pr-note" style="color:var(--warning)">dfu-util isn't installed, so this chip can't be flashed over DFU yet (apt package ${esc(b.tool.pkg)}).</div>`;
      const d = q(k.el, '.pb-dfu'); if (d) d.style.display = b.tool.path ? '' : 'none';
    }
    q(k.el, '.pb-status').innerHTML = h;
    q(k.el, '.pb-job').innerHTML = jobHtml(b);
    for (const el of k.el.querySelectorAll('.pb-act')) el.disabled = !!b.job;
  }

  // ---------- Card lifecycle ----------

  function openLog(k, id) {
    const pre = q(k.el, '.pb-log');
    const add = text => { const atEnd = pre.scrollTop + pre.clientHeight >= pre.scrollHeight - 4; pre.textContent += text + '\n'; if (pre.textContent.length > 200000) pre.textContent = pre.textContent.slice(-150000); if (atEnd) pre.scrollTop = pre.scrollHeight; };
    k.ws = pws(`/ws/probes/${enc(id)}`, {
      onmessage(m) {
        if (m?.type === 'log') { pre.textContent = ''; for (const l of m.lines) add(l); }
        else if (m?.type === 'line') add(m.text);
        else if (m?.type === 'job') q(k.el, '.pb-job').innerHTML = jobHtml(m);
      },
    });
  }

  function ensureCard(d, kind, host) {
    let k = cards[d.id];
    if (k) return k;
    const el = document.createElement('div');
    el.className = 'pr-card';
    el.dataset.id = d.id;
    el.innerHTML = kind === 'probe' ? probeSkeleton(d) : bootSkeleton(d);
    host.appendChild(el);
    k = cards[d.id] = { el, kind, ws: null, clientsFor: -1 };
    if (q(el, '.pb-log')) openLog(k, d.id);
    // Config edits save immediately; the server keeps the source of truth.
    el.addEventListener('change', e => {
      const t = e.target;
      if (t.matches('.pb-target')) pbConfig(d.id, { target: t.value });
      else if (t.matches('.pb-transport')) pbConfig(d.id, { transport: t.value });
      else if (t.matches('.pb-speed')) pbConfig(d.id, { speed: +t.value });
      else if (t.matches('.pb-port')) pbConfig(d.id, { ports: { [t.dataset.k]: +t.value } });
    });
    return k;
  }

  function dropCard(id) {
    const k = cards[id]; if (!k) return;
    k.ws?.close(); k.el.remove(); delete cards[id];
  }

  function render(st) {
    if (!root) return;
    const pHost = q(root, '.pb-probes'), bHost = q(root, '.pb-boots');
    const seen = new Set();
    for (const p of st.probes) { seen.add(p.id); updateProbe(ensureCard(p, 'probe', pHost), p); }
    for (const b of st.bootloaders) { seen.add(b.id); updateBoot(ensureCard(b, 'boot', bHost), b); }
    for (const id of Object.keys(cards)) if (!seen.has(id)) dropCard(id);
    const empty = !st.probes.length && !st.bootloaders.length;
    q(root, '.pb-empty').style.display = empty ? '' : 'none';
    const oc = q(root, '.pb-ocd');
    oc.innerHTML = st.openocd.path ? `OpenOCD ${esc(st.openocd.version)}` : `<span style="color:var(--warning)">OpenOCD isn't installed (apt package openocd); probes are detected but can't be used.</span>`;
  }

  // ---------- Actions (onclick targets) ----------

  const card = id => cards[id]?.el;
  const showOut = (id, text) => { const o = q(card(id), '.pb-out'); o.style.display = text ? '' : 'none'; o.textContent = text || ''; };

  window.pbConfig = async (id, body) => { try { await papi('PUT', `/api/probes/${enc(id)}/config`, body); } catch {} };
  window.pbStart = async id => {
    const b = q(card(id), '.pb-start'); b.disabled = true; b.textContent = 'Starting…';
    try { await papi('POST', `/api/probes/${enc(id)}/start`, {}); toast('GDB server running', 'success'); } catch {}
    finally { b.disabled = false; b.textContent = 'Start GDB server'; }
  };
  window.pbStop = async id => { try { await papi('POST', `/api/probes/${enc(id)}/stop`); } catch {} };
  window.pbAct = async (id, verb, body) => {
    try {
      const r = await papi('POST', `/api/probes/${enc(id)}/${verb}`, body);
      if (verb === 'examine') showOut(id, [r.cpus.length ? 'Cores: ' + r.cpus.join(', ') : 'No core detected', r.targets].filter(Boolean).join('\n\n'));
      else if (r.output) showOut(id, r.output);
    } catch {}
  };
  window.pbErase = id => {
    showModal(`<h3>Mass erase?</h3><p class="pr-note">This erases the target's whole flash, including the firmware and any stored data.</p>
      <div class="modal-btns"><button class="btn" onclick="hideModal()">Cancel</button><button class="btn btn-danger" onclick="hideModal();pbAct('${esc(id)}','erase')">Erase</button></div>`);
  };
  window.pbFlash = async id => {
    const el = card(id), f = q(el, '.pb-file').files[0];
    if (!f) return toast('Choose a firmware file first', 'error');
    const qs = new URLSearchParams({ name: f.name, verify: q(el, '.pb-verify').checked ? '1' : '0', reset: q(el, '.pb-reset').checked ? '1' : '0' });
    const a = q(el, '.pb-addr').value.trim(); if (a) qs.set('address', a);
    q(el, '.pb-logbox').open = true;
    try { const r = await upload(`/api/probes/${enc(id)}/flash?${qs}`, f); toast(r.message || 'Flashed', 'success'); showOut(id, r.output || ''); } catch {}
  };
  window.pbRead = async id => {
    const el = card(id);
    const qs = new URLSearchParams({ address: q(el, '.pb-maddr').value.trim(), length: q(el, '.pb-mlen').value });
    try { const r = await papi('GET', `/api/probes/${enc(id)}/memory?${qs}`); showOut(id, r.dump || '(no data)'); } catch {}
  };
  window.pbUf2 = async id => {
    const el = card(id), f = q(el, '.pb-file').files[0];
    if (!f) return toast('Choose a .uf2 file first', 'error');
    try { const r = await upload(`/api/probes/boot/${enc(id)}/uf2?name=${enc(f.name)}`, f); toast(`${r.message} (${fmtBytes(r.payload)})`, 'success'); } catch {}
  };
  window.pbDfu = async id => {
    const el = card(id), f = q(el, '.pb-file').files[0];
    if (!f) return toast('Choose a .bin file first', 'error');
    q(el, '.pb-logbox').open = true;
    try { const r = await upload(`/api/probes/boot/${enc(id)}/dfu?name=${enc(f.name)}&address=${enc(q(el, '.pb-addr').value.trim())}`, f); toast(r.message, 'success'); } catch {}
  };

  periphRegister('probes', {
    mount(el, st) {
      root = el;
      el.innerHTML = `<div class="pr-note pb-ocd"></div>
        <div class="pb-empty pr-note" style="margin-top:8px">No debug probe or bootloader-mode MCU is connected. Plug in an ST-Link, J-Link, CMSIS-DAP (e.g. Raspberry Pi Debug Probe), ESP-Prog or Black Magic Probe, or hold BOOTSEL while plugging in a Pico.</div>
        <div class="pb-probes"></div><div class="pb-boots"></div>`;
      loadTargets();
      render(st);
    },
    update(st) { render(st); },
    unmount() { for (const id of Object.keys(cards)) dropCard(id); root = null; },
  });
})();
