// ==================== USB panel ====================
// Device tree (controllers → buses → hubs → devices) with per-port power chips, USB/IP and
// usbredir sharing, and the hardware timeline. Server: peripherals/drivers/usb.ts.
(() => {
  const PANELS = { serial: 'Serial', can: 'CAN', camera: 'Camera', drives: 'Drives', netdiag: 'Network', wifi: 'Wi-Fi', i2c: 'I²C', gpio: 'GPIO', ble: 'Bluetooth', sdr: 'SDR', gps: 'GPS', probes: 'Debug probes', ups: 'UPS' };
  const CSS = `
.usb-tree{font-size:.8rem;display:flex;flex-direction:column;gap:2px}
.usb-ctl{font-size:.72rem;font-weight:700;text-transform:uppercase;letter-spacing:.06em;color:var(--dim);margin:10px 0 2px}
.usb-ctl:first-child{margin-top:0}
.usb-dev{border-left:2px solid var(--border);padding:3px 0 3px 8px;min-width:0}
.usb-dev.root{border-left-color:var(--surface3)}
.usb-l1{display:flex;flex-wrap:wrap;gap:4px 8px;align-items:baseline}
.usb-l1 b{font-weight:600;overflow-wrap:anywhere}
.usb-l2{font-size:.72rem;color:var(--dim);margin-top:1px;overflow-wrap:anywhere}
.usb-l2 .nd{color:var(--danger)}
.usb-pn{font-family:'SF Mono',ui-monospace,monospace;font-size:.7rem;color:var(--dim)}
.usb-ports{display:flex;flex-wrap:wrap;gap:4px;margin:3px 0 1px}
.usb-chip{font:inherit;font-size:.68rem;padding:1px 7px;border-radius:9px;border:1px solid var(--border);background:var(--surface2);color:var(--dim);cursor:pointer}
.usb-chip.on{color:var(--success);border-color:rgba(76,175,125,.4)}
.usb-chip.off{color:var(--dim);border-style:dashed}
.usb-chip.oc{color:var(--danger);border-color:var(--danger)}
.usb-chip.int{cursor:default;opacity:.7}
.usb-chip.sel{outline:1px solid var(--accent)}
.usb-chip:disabled{cursor:default}
.usb-bar{display:flex;flex-wrap:wrap;gap:6px;align-items:center;margin:4px 0;padding:6px 8px;background:var(--surface2);border:1px solid var(--border);border-radius:6px;font-size:.75rem}
.usb-bar input{width:3.2em;background:var(--surface);border:1px solid var(--border);color:var(--text);border-radius:4px;padding:2px 4px;font:inherit}
.usb-more{margin:4px 0;padding:6px 8px;background:var(--surface2);border-radius:6px;font-size:.75rem;display:flex;flex-direction:column;gap:4px}
.usb-link{background:none;border:none;color:var(--accent);cursor:pointer;font:inherit;padding:0;text-decoration:underline}
.usb-ev{display:flex;gap:6px;align-items:baseline;padding:1px 0}
.usb-ev .t{color:var(--dim);flex:none}
.usb-ev.error{color:var(--danger)}
.usb-ev.warn{color:var(--warning)}
.usb-filt{display:inline-flex;gap:4px;margin-left:auto}
.usb-cmd{display:flex;flex-wrap:wrap;gap:4px 8px;align-items:center}
`;
  let el = null, st = null, sel = null, tlKind = 'all', hist = null;
  const open = new Set();
  const sig = {};

  const devName = d => [d.vendor, d.product].filter(Boolean).join(' ') || d.vidpid;
  const pill = (t, cls = '') => `<span class="pr-pill ${cls}">${esc(t)}</span>`;
  const btn = (act, label, data = {}, cls = '') => `<button class="btn btn-sm ${cls}" data-act="${act}"${Object.entries(data).map(([k, v]) => ` data-${k}="${esc(v)}"`).join('')}>${esc(label)}</button>`;
  const panelLink = (p, dev = '') => !p ? '' : (p === 'drives' || (typeof PERIPH !== 'undefined' && PERIPH[p])) ? ` → <button class="usb-link" data-act="panel" data-panel="${esc(p)}" data-dev="${esc(dev)}">${esc(PANELS[p] || p)} panel</button>` : ` → ${esc(PANELS[p] || p)}`;

  function portChips(d) {
    const pw = d.power;
    if (!pw) return '';
    if (!pw.switchable || !pw.ports) {
      const used = d.ports.filter(p => p.child).length;
      return `<div class="usb-l2">${d.nports} port${d.nports === 1 ? '' : 's'}, ${used} in use · ${esc(pw.why || 'power not switchable')}</div>`;
    }
    const grouped = !!pw.group;
    const chips = pw.ports.map(p => {
      const child = d.ports.find(x => x.port === p.port)?.child;
      const oc = p.overCurrent || d.ports.find(x => x.port === p.port)?.overCurrent;
      const cls = p.internal ? 'int' : oc ? 'oc' : p.power ? 'on' : 'off';
      const state = p.internal ? 'internal' : oc ? 'over-current' : p.power ? 'on' : 'off';
      const isSel = sel && sel.hub === pw.loc && sel.port === p.port;
      const dis = p.internal || grouped ? 'disabled' : '';
      return `<button class="usb-chip ${cls}${isSel ? ' sel' : ''}" data-act="chip" data-hub="${esc(pw.loc)}" data-port="${p.port}" ${dis}>P${p.port} ${state}${child ? ' · used' : ''}</button>`;
    }).join('');
    let bar = '';
    if (grouped && pw.group.hubs[0] === pw.loc) {
      bar = `<div class="usb-bar"><span>⚠ ${esc(pw.group.label)}.</span>${powerBtns({ hub: pw.loc })}</div>`;
    } else if (grouped) {
      bar = `<div class="usb-l2">Ganged with hub ${esc(pw.group.hubs[0])}: same sockets, switched there.</div>`;
    } else if (sel && sel.hub === pw.loc) {
      const p = pw.ports.find(x => x.port === sel.port);
      if (p) bar = `<div class="usb-bar"><b>Hub ${esc(pw.loc)} port ${p.port}</b>${powerBtns({ hub: pw.loc, port: p.port })}</div>`;
    }
    return `<div class="usb-ports">${chips}</div>${bar}`;
  }

  function powerBtns(t) {
    const data = { hub: t.hub, ...(t.port != null ? { port: t.port } : {}), ...(t.busid ? { busid: t.busid } : {}) };
    return `${btn('power', 'On', { ...data, action: 'on' })}${btn('power', 'Off', { ...data, action: 'off' }, 'btn-warn')}${btn('power', 'Cycle', { ...data, action: 'cycle' })}<label class="pr-note">off for <input type="number" min="1" max="30" value="2" data-delay> s</label>`;
  }

  function devRow(d) {
    const depth = Math.min(d.depth, 6);
    const style = depth ? ` style="margin-left:${depth * 12}px"` : '';
    if (d.root) {
      const ver = d.speed >= 5000 ? 'USB 3' : 'USB 2';
      return `<div class="usb-dev root"${style}><div class="usb-l1"><b>Bus ${d.bus}</b><span class="pr-note">${ver} root hub · ${esc(d.speedLabel)} · ${d.nports} port${d.nports === 1 ? '' : 's'}</span>${d.power?.mode ? pill(d.power.group ? 'ganged' : d.power.mode) : ''}</div>${portChips(d)}</div>`;
    }
    const pills = [];
    if (d.speedLabel) pills.push(pill(d.speedLabel));
    if (d.usbip) pills.push(pill(d.usbip === 'in use' ? 'USB/IP: in use by remote' : d.usbip === 'error' ? 'USB/IP: error' : 'USB/IP: shared, available', d.usbip === 'error' ? 'bad' : 'on'));
    if (d.redir) pills.push(pill(`usbredir :${d.redir.port}${d.redir.peers.length ? ' · ' + d.redir.peers.join(', ') : ''}`, 'on'));
    if (d.protect.length) pills.push(pill('protected', 'warn'));
    if (d.hub && d.power?.mode) pills.push(pill(d.power.group ? 'ganged' : d.power.switchable ? d.power.mode : 'no switching'));
    const meta = [d.vidpid, d.version && `USB ${d.version}`, d.maxPowerMa ? `${d.maxPowerMa} mA` : '', d.config ? `cfg ${d.config}` : '', d.hub ? `${d.nports} ports` : ''].filter(Boolean).join(' · ');
    let l2 = '';
    if (!d.hub) {
      const ifs = d.interfaces.map(i => `${esc(i.clsName)} → ${i.driver ? esc(i.driver) : '<span class="nd">no driver</span>'}`).join(' · ');
      const nodes = d.nodes.map(n => `${esc(n.dev)}${n.label ? ` (${esc(n.label)})` : ''}${panelLink(n.panel, n.dev)}`).join(' · ');
      const hint = d.hint ? `${esc(d.hint.label)}${panelLink(d.hint.panel)}` : '';
      const usbipNote = d.usbip ? 'Detached from local drivers while shared' : '';
      l2 = [ifs, nodes, hint, usbipNote].filter(Boolean).map(x => `<div class="usb-l2">${x}</div>`).join('');
    }
    const actions = [];
    if (!d.hub) {
      if (d.usbip) actions.push(btn('unshare', 'Unshare', { busid: d.busid }));
      else if (!d.redir && !d.shareBlock) actions.push(btn('share', 'Share (USB/IP)', { busid: d.busid }));
      actions.push(btn('more', open.has(d.busid) ? 'Less' : 'More', { busid: d.busid }));
    }
    let more = '';
    if (open.has(d.busid)) more = moreBox(d);
    return `<div class="usb-dev"${style}><div class="usb-l1"><span class="usb-pn">${esc(d.busid)}</span><b>${esc(devName(d))}</b><span class="pr-note">${esc(meta)}</span>${pills.join('')}${actions.join('')}</div>${l2}${d.protect.length ? `<div class="usb-l2">Protected: this device ${esc(d.protect.join('; '))}</div>` : ''}${d.hub ? portChips(d) : ''}${more}</div>`;
  }

  function moreBox(d) {
    const rows = [];
    rows.push(`<div>${d.serial ? `Serial <span class="pr-endpoint">${esc(d.serial)}</span> · ` : ''}Class ${esc(d.cls)} · driver ${esc(d.driver)} · device ${d.bus}-${d.devnum}${d.idPath ? ` · <span class="pr-endpoint">${esc(d.idPath)}</span>` : ''}</div>`);
    const vp = d.rules.includes('vidpid'), pt = d.rules.includes('port');
    rows.push(`<div class="usb-cmd">Auto-share: <label><input type="checkbox" data-act="rule" data-match="vidpid" data-value="${esc(d.vidpid)}" ${vp ? 'checked' : ''}> every ${esc(d.vidpid)}</label><label><input type="checkbox" data-act="rule" data-match="port" data-value="${esc(d.busid)}" ${pt ? 'checked' : ''}> anything on port ${esc(d.busid)}</label></div>`);
    if (d.shareBlock && !d.usbip) rows.push(`<div class="pr-note">${esc(d.shareBlock)}</div>`);
    if (st.redir.available) {
      if (d.redir) rows.push(`<div class="usb-cmd">usbredir on TCP ${d.redir.port} ${btn('redir-stop', 'Stop usbredir', { busid: d.busid })}</div><div class="usb-cmd">QEMU <span class="pr-endpoint">-chardev socket,id=redir0,host=${esc(clientHost())},port=${d.redir.port} -device usb-redir,chardev=redir0</span></div>`);
      else if (!d.usbip && !d.shareBlock) rows.push(`<div class="usb-cmd">${btn('redir-start', 'Serve with usbredir', { busid: d.busid })}<span class="pr-note">for QEMU/libvirt usb-redir; exclusive with USB/IP</span></div>`);
    } else rows.push(`<div class="pr-note">usbredir unavailable: install the <span class="pr-endpoint">${esc(st.redir.apt)}</span> package</div>`);
    if (d.upstream) rows.push(`<div class="usb-cmd">Port power (hub ${esc(d.upstream.hub)} port ${d.upstream.port}): ${powerBtns({ busid: d.busid })}</div>`);
    return `<div class="usb-more">${rows.join('')}</div>`;
  }

  function renderTree() {
    const out = [];
    const shown = new Set();
    for (const c of st.controllers) {
      out.push(`<div class="usb-ctl">${esc(c.name)} · ${esc(c.driver)} · ${esc(c.id)}</div>`);
      for (const d of st.devices) {
        if (!c.buses.includes(d.bus)) continue;
        shown.add(d.busid);
        out.push(devRow(d));
      }
    }
    const rest = st.devices.filter(d => !shown.has(d.busid));
    if (rest.length) out.push('<div class="usb-ctl">Other</div>', ...rest.map(devRow));
    if (!st.uhubctl.available) out.push(`<div class="pr-note">Port power switching needs <span class="pr-endpoint">uhubctl</span> (apt: uhubctl).</div>`);
    else if (st.uhubctl.error) out.push(`<div class="pr-note">uhubctl: ${esc(st.uhubctl.error)}</div>`);
    return out.join('');
  }

  // The name this dashboard was reached by is the one that resolves from the client's side;
  // fall back to the server's own name when we were reached by a bare IP.
  function clientHost() { const h = location.hostname; return !h || /^[\d.]+$|:/.test(h) ? st.host : h; }

  function renderShare() {
    const host = clientHost();
    const u = st.usbip;
    const shared = st.devices.filter(d => d.usbip);
    const head = u.running ? `usbipd on TCP ${u.port}${u.peers.length ? ' · clients ' + u.peers.join(', ') : ' · no clients'}` : u.available ? `usbipd starts on TCP ${u.port} when a device is shared` : 'usbip is not installed (apt: usbip)';
    const pf = u.port === 3240 ? '' : ` --tcp-port ${u.port}`;
    const b = shared[0]?.busid || '<busid>';
    const rules = st.rules.length ? st.rules.map(r => `<span class="pr-pill">${r.match === 'port' ? 'port ' : ''}${esc(r.value)} <button class="pr-x" data-act="rule-del" data-match="${esc(r.match)}" data-value="${esc(r.value)}" aria-label="Remove rule">&times;</button></span>`).join(' ') : '<span class="pr-note">none</span>';
    return `<h4>Network sharing <small>${esc(head)}</small></h4>
      ${u.error ? `<div class="pr-note" style="color:var(--danger)">${esc(u.error)}</div>` : ''}
      <div class="pr-note">Sharing detaches the device from BenchBob: it disappears from the Serial, CAN, Drives and other panels until you unshare it.</div>
      <div class="usb-cmd pr-note">Linux client: <span class="pr-endpoint">usbip${esc(pf)} list -r ${esc(host)}</span> <span class="pr-endpoint">sudo usbip${esc(pf)} attach -r ${esc(host)} -b ${esc(b)}</span></div>
      <div class="usb-cmd pr-note">Windows: install usbip-win2, then <span class="pr-endpoint">usbip.exe${esc(pf)} attach -r ${esc(host)} -b ${esc(b)}</span></div>
      <div class="usb-cmd pr-note">usbredir: ${st.redir.available ? `available; sessions start at TCP ${st.redir.basePort}` : `not installed (apt: <span class="pr-endpoint">${esc(st.redir.apt)}</span>)`}</div>
      <div class="usb-cmd pr-note">Auto-share rules: ${rules}</div>
      <div class="pr-row"><label>usbipd port <input type="number" data-cfg="usbipPort" value="${u.port}" style="width:6em"></label><label>usbredir first port <input type="number" data-cfg="redirBasePort" value="${st.redir.basePort}" style="width:6em"></label>${btn('cfg', 'Save ports')}</div>`;
  }

  function renderTimeline() {
    const kinds = { all: null, usb: ['plug', 'unplug', 'power', 'share'], kernel: ['kernel'] }[tlKind];
    let rows = (st.timeline || []).filter(e => !kinds || kinds.includes(e.detail?.kind));
    const fmt = t => new Date(t).toLocaleTimeString([], { hour: '2-digit', minute: '2-digit', second: '2-digit' });
    let html = rows.map(e => `<div class="usb-ev ${esc(e.detail?.level || '')}"><span class="t">${fmt(e.t)}</span>${e.source !== 'usb' ? pill(e.source) : e.detail?.kind ? pill(e.detail.kind) : ''}<span>${esc(e.text)}</span></div>`).join('');
    if (!html) html = '<div class="pr-note">Nothing yet. Plug something in.</div>';
    if (hist) html += `<div class="pr-note" style="margin-top:6px">Kernel log since boot (historical, newest first)</div>` + hist.slice().reverse().map(k => `<div class="usb-ev ${esc(k.level)}"><span class="t">${esc(k.ts.slice(11, 19))}</span><span>${esc(k.text)}</span></div>`).join('');
    const f = (k, l) => `<button class="btn btn-sm${tlKind === k ? ' btn-primary' : ''}" data-act="tl" data-kind="${k}">${l}</button>`;
    return `<h4>Timeline <small>newest first</small><span class="usb-filt">${f('all', 'All')}${f('usb', 'USB')}${f('kernel', 'Kernel')}<button class="btn btn-sm" data-act="hist">${hist ? 'Hide boot log' : 'Since boot'}</button></span></h4><div class="pr-log" style="max-height:240px">${html}</div>${st.kernelLog?.error ? `<div class="pr-note">${esc(st.kernelLog.error)}</div>` : ''}`;
  }

  function put(id, html) {
    if (sig[id] === html) return;
    const n = el.querySelector('#usb-' + id);
    if (!n) return;
    // Keep a half-typed port number when the 2 s status push lands; retry on the next push.
    if (n.contains(document.activeElement) && document.activeElement.tagName === 'INPUT') return;
    sig[id] = html;
    n.innerHTML = html;
  }

  function render() {
    if (!el || !st || !st.devices) return;
    put('tree', renderTree());
    put('share', renderShare());
    put('tl', renderTimeline());
  }

  // ---------- actions ----------

  async function doPower(b) {
    const args = { action: b.dataset.action };
    if (b.dataset.busid) args.busid = b.dataset.busid;
    else { args.hub = b.dataset.hub; if (b.dataset.port) args.port = +b.dataset.port; }
    const delayIn = b.parentElement.querySelector('[data-delay]');
    if (delayIn) args.delay = +delayIn.value || 2;
    let plan;
    try { plan = await papi('GET', '/api/usb/power/plan?' + new URLSearchParams(args)); } catch { return; }
    if (plan.needs !== 'none' || (args.action !== 'on' && plan.devices.length)) {
      const level = await confirmPower(plan);
      if (!level) return;
      if (plan.needs !== 'none') args.confirm = plan.needs;
    }
    toast(`Power ${args.action}…`);
    try { await papi('POST', '/api/usb/power', args); toast(`Power ${args.action} done`, 'success'); } catch {}
  }

  function confirmPower(plan) {
    return new Promise(resolve => {
      const unsafe = plan.needs === 'unsafe';
      const word = 'CUT POWER';
      showModal(`<h3${unsafe ? ' style="color:var(--danger)"' : ''}>Switch port power ${esc(plan.action)}?</h3>
        ${plan.warnings.map(w => `<p>${esc(w)}</p>`).join('')}
        <div class="modal-dev">Ports: ${esc(plan.ports.join(', '))}<br>Devices losing power: ${esc(plan.devices.join(', ') || 'none')}</div>
        ${unsafe ? `<div class="confirm-box"><label>Type ${word} to confirm:</label><input type="text" id="usb-cw" autocomplete="off"></div>` : ''}
        <div class="modal-btns"><button class="btn" id="mc">Cancel</button><button class="btn ${unsafe ? 'btn-danger' : 'btn-warn'}" id="mk" ${unsafe ? 'disabled' : ''}>${plan.group ? `Switch all ${plan.ports.length} ports` : 'Switch'}</button></div>`);
      if (unsafe) Q('#usb-cw').addEventListener('input', () => { Q('#mk').disabled = Q('#usb-cw').value !== word; });
      Q('#mc').onclick = () => { hideModal(); resolve(null); };
      Q('#mk').onclick = () => { hideModal(); resolve(plan.needs); };
      Q('#modal-bg').onclick = e => { if (e.target === Q('#modal-bg')) { hideModal(); resolve(null); } };
    });
  }

  async function onClick(e) {
    const b = e.target.closest('[data-act]');
    if (!b || !el.contains(b) || b.type === 'checkbox') return;
    const d = b.dataset;
    switch (d.act) {
      case 'chip': sel = sel && sel.hub === d.hub && sel.port === +d.port ? null : { hub: d.hub, port: +d.port }; sig.tree = null; render(); break;
      case 'power': doPower(b); break;
      case 'more': open.has(d.busid) ? open.delete(d.busid) : open.add(d.busid); sig.tree = null; render(); break;
      case 'panel':
        if (d.panel !== 'drives') periphShow(d.panel);
        else if (document.getElementById('drive-' + d.dev.replace('/dev/', ''))) scrollToDrive(d.dev.replace('/dev/', ''));
        else Q('#drives')?.scrollIntoView({ behavior: 'smooth' });
        break;
      case 'share': b.disabled = true; try { const r = await papi('POST', '/api/usb/share', { busid: d.busid }); toast(`Shared ${d.busid}. Attach with: ${r.attach}`, 'success'); } catch {} b.disabled = false; break;
      case 'unshare': b.disabled = true; try { await papi('POST', '/api/usb/unshare', { busid: d.busid }); toast(`Stopped sharing ${d.busid}`, 'success'); } catch {} b.disabled = false; break;
      case 'redir-start': try { const r = await papi('POST', '/api/usb/redir/start', { busid: d.busid }); toast(`usbredir on TCP ${r.port}`, 'success'); } catch {} break;
      case 'redir-stop': try { await papi('POST', '/api/usb/redir/stop', { busid: d.busid }); } catch {} break;
      case 'rule-del': try { await papi('DELETE', '/api/usb/rules', { match: d.match, value: d.value }); } catch {} break;
      case 'tl': tlKind = d.kind; sig.tl = null; render(); break;
      case 'hist':
        if (hist) hist = null;
        else try { hist = (await papi('GET', '/api/usb/dmesg')).lines; } catch {}
        sig.tl = null; render(); break;
      case 'cfg': {
        const body = {};
        el.querySelectorAll('[data-cfg]').forEach(i => body[i.dataset.cfg] = +i.value);
        try { await papi('POST', '/api/usb/config', body); toast('Ports saved', 'success'); } catch {}
        break;
      }
    }
  }

  async function onChange(e) {
    const c = e.target;
    if (c.dataset?.act !== 'rule') return;
    try { await papi(c.checked ? 'POST' : 'DELETE', '/api/usb/rules', { match: c.dataset.match, value: c.dataset.value }); } catch { c.checked = !c.checked; }
  }

  periphRegister('usb', {
    mount(e, s) {
      el = e;
      if (!document.getElementById('usb-css')) { const t = document.createElement('style'); t.id = 'usb-css'; t.textContent = CSS; document.head.appendChild(t); }
      el.innerHTML = `<div class="usb-tree" id="usb-tree"></div><div class="pr-card" id="usb-share"></div><div class="pr-card" id="usb-tl"></div>`;
      for (const k in sig) delete sig[k];
      el.addEventListener('click', onClick);
      el.addEventListener('change', onChange);
      st = s; render();
    },
    update(s) { st = s; render(); },
    unmount() { el = null; },
  });
})();
