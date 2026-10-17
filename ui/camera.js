// Camera section: one card per camera with an opt-in live view, snapshot/download, stream
// settings and image controls. The live <img> only exists while the user has asked for it
// AND the card is on screen in a visible tab, because every open view keeps the camera
// (and ustreamer) running and costs tens of Mbit/s.
(() => {
  let root = null;
  const cards = {}; // id -> { el, live, detail, io, visible }
  const LS = 'cameraLive';
  let liveWanted = {};
  try { liveWanted = JSON.parse(localStorage.getItem(LS) || '{}'); } catch {}
  const saveLive = () => { try { localStorage.setItem(LS, JSON.stringify(liveWanted)); } catch {} };

  const kindPill = c => c.kind === 'hdmi' ? '<span class="pr-pill">HDMI capture</span>' : '<span class="pr-pill">webcam</span>';
  const fmtStream = s => s ? `${esc(s.fourcc)} ${s.w}×${s.h} @ ${s.fps} fps` : 'no streamable format';

  function setLive(id) {
    const k = cards[id]; if (!k) return;
    const on = !!liveWanted[id] && k.visible && document.visibilityState === 'visible';
    const box = k.el.querySelector('.cam-view');
    const img = box.querySelector('img.cam-live');
    if (on && !img) {
      box.innerHTML = `<img class="cam-live" alt="Live view" style="width:100%;max-height:70vh;object-fit:contain;background:#000;border-radius:4px;display:block" src="/api/camera/${encodeURIComponent(id)}/stream?t=${Date.now()}">`;
      box.querySelector('img').onerror = () => { box.innerHTML = '<div class="pr-note">Stream ended. Press Live view to retry.</div>'; liveWanted[id] = false; saveLive(); syncButtons(id); };
    } else if (!on && img) {
      img.src = ''; // closes the HTTP stream so the server-side idle timer can stop the camera
      box.innerHTML = liveWanted[id] ? '<div class="pr-note">Live view paused while off screen.</div>' : '';
    }
    syncButtons(id);
  }
  function syncButtons(id) {
    const b = cards[id]?.el.querySelector('.cam-livebtn');
    if (b) b.textContent = liveWanted[id] ? 'Stop live view' : 'Live view';
  }

  window.camToggleLive = id => { liveWanted[id] = !liveWanted[id]; saveLive(); setLive(id); };

  window.camSnapshot = async id => {
    const k = cards[id]; if (!k) return;
    const out = k.el.querySelector('.cam-snap');
    out.innerHTML = '<div class="pr-note">Capturing…</div>';
    try {
      const r = await fetch(`/api/camera/${encodeURIComponent(id)}/snapshot`);
      if (!r.ok) throw new Error((await r.json().catch(() => ({}))).message || r.statusText);
      const url = URL.createObjectURL(await r.blob());
      if (k.snapUrl) URL.revokeObjectURL(k.snapUrl);
      k.snapUrl = url;
      const name = `${id}-${new Date().toISOString().replace(/[:.]/g, '-')}.jpg`;
      out.innerHTML = `<div class="pr-row"><span class="pr-note">Snapshot ${new Date().toLocaleTimeString()}</span><a class="btn btn-sm" href="${url}" download="${esc(name)}">Download</a><button class="btn btn-sm" onclick="this.closest('.cam-snap').innerHTML=''">Close</button></div><img alt="Snapshot" src="${url}" style="width:100%;max-height:50vh;object-fit:contain;background:#000;border-radius:4px;display:block">`;
    } catch (e) { out.innerHTML = `<div class="pr-note" style="color:var(--danger)">${esc(e.message)}</div>`; }
  };

  async function loadDetail(id) {
    const k = cards[id]; if (!k) return;
    try { k.detail = await papi('GET', `/api/camera/${encodeURIComponent(id)}`); } catch { return; }
    renderSettings(id);
    renderControls(id);
  }

  function renderSettings(id) {
    const k = cards[id], d = k?.detail; if (!d) return;
    const fmts = d.formats.filter(f => ['MJPG', 'JPEG', 'YUYV', 'UYVY', 'RGBP', 'RGB3'].includes(f.fourcc));
    const cur = d.stream || {};
    const f = fmts.find(x => x.fourcc === cur.fourcc) || fmts[0];
    const sizes = f ? f.sizes : [];
    const size = sizes.find(s => s.w === cur.w && s.h === cur.h) || sizes[0];
    const fpsList = size ? size.fps : [];
    k.el.querySelector('.cam-settings').innerHTML = !fmts.length ? '<div class="pr-note">This camera offers no format ustreamer can stream.</div>' : `
      <div class="pr-row">
        <label>Format <select class="cam-fmt">${fmts.map(x => `<option value="${esc(x.fourcc)}" ${x === f ? 'selected' : ''}>${esc(x.fourcc)} · ${esc(x.description)}</option>`).join('')}</select></label>
        <label>Size <select class="cam-size">${sizes.map(s => `<option value="${s.w}x${s.h}" ${s === size ? 'selected' : ''}>${s.w}×${s.h}</option>`).join('')}${!sizes.length && f?.stepwise ? `<option>${cur.w}x${cur.h}</option>` : ''}</select></label>
        <label>FPS <select class="cam-fps">${fpsList.map(v => `<option ${v === cur.fps ? 'selected' : ''}>${v}</option>`).join('')}${!fpsList.length ? `<option>${cur.fps || 30}</option>` : ''}</select></label>
        <button class="btn btn-sm" onclick="camApplyStream('${esc(id)}')">Apply</button>
      </div>
      <div class="pr-note">${f && !f.compressed ? 'Uncompressed: the Pi JPEG-encodes every frame, so keep it modest.' : 'MJPEG is forwarded as the camera sends it.'}</div>
      <div class="pr-row">
        <label><input type="checkbox" class="cam-share" ${d.share ? 'checked' : ''} onchange="camShare('${esc(id)}')"> Share directly on port</label>
        <input type="number" class="cam-port" min="1024" max="65535" value="${d.sharePort}" style="width:6em" onchange="camShare('${esc(id)}')">
      </div>
      <div class="pr-note">${d.share ? `ustreamer listens on all interfaces: <span class="pr-endpoint">http://${esc(location.hostname)}:${d.sharePort}/stream</span> (also /snapshot, /state). It stays running until you turn this off.` : 'Off: the camera is reachable only through DeskyPi and sleeps when nobody watches.'}</div>`;
    // Re-filter the size/fps lists when the format or size changes, without applying yet.
    const el = k.el.querySelector('.cam-settings');
    el.querySelector('.cam-fmt')?.addEventListener('change', e => { k.detail.stream = { ...cur, fourcc: e.target.value, w: 0, h: 0 }; renderSettings(id); });
    el.querySelector('.cam-size')?.addEventListener('change', e => { const [w, h] = e.target.value.split('x').map(Number); k.detail.stream = { ...k.detail.stream, fourcc: f.fourcc, w, h }; renderSettings(id); });
  }

  window.camApplyStream = async id => {
    const el = cards[id]?.el.querySelector('.cam-settings'); if (!el) return;
    const [w, h] = el.querySelector('.cam-size').value.split('x').map(Number);
    await papi('POST', `/api/camera/${encodeURIComponent(id)}/config`, { fourcc: el.querySelector('.cam-fmt').value, w, h, fps: Number(el.querySelector('.cam-fps').value) }).catch(() => {});
    // The old stream was cut by the restart; reopen it if the user was watching.
    const box = cards[id].el.querySelector('.cam-view'); box.innerHTML = ''; setLive(id);
    loadDetail(id);
  };
  window.camShare = async id => {
    const el = cards[id]?.el.querySelector('.cam-settings'); if (!el) return;
    await papi('POST', `/api/camera/${encodeURIComponent(id)}/config`, { share: el.querySelector('.cam-share').checked, sharePort: Number(el.querySelector('.cam-port').value) }).catch(() => {});
    const box = cards[id].el.querySelector('.cam-view'); box.innerHTML = ''; setLive(id);
    loadDetail(id);
  };

  function renderControls(id) {
    const k = cards[id], d = k?.detail; if (!d) return;
    const ctl = d.controls;
    const row = c => {
      const inactive = c.flags.includes('inactive');
      const dim = inactive ? 'opacity:.5' : '';
      const note = inactive ? ' <span class="pr-note">(set by its auto mode)</span>' : '';
      const label = esc(c.name.replace(/_/g, ' '));
      let input;
      if (c.type === 'bool') input = `<input type="checkbox" ${c.value ? 'checked' : ''} onchange="camCtl('${esc(id)}','${esc(c.name)}',this.checked?1:0)">`;
      else if (c.menu) input = `<select onchange="camCtl('${esc(id)}','${esc(c.name)}',this.value)">${c.menu.map(m => `<option value="${m.value}" ${m.value === c.value ? 'selected' : ''}>${esc(m.label)}</option>`).join('')}</select>`;
      else input = `<input type="range" min="${c.min}" max="${c.max}" step="${c.step || 1}" value="${c.value}" style="flex:1;min-width:120px" oninput="this.nextElementSibling.textContent=this.value" onchange="camCtl('${esc(id)}','${esc(c.name)}',this.value)"><span style="min-width:3.5em;text-align:right;font-variant-numeric:tabular-nums">${c.value}</span>`;
      return `<div class="pr-row" style="${dim};margin:4px 0"><span style="min-width:11em;font-size:.8rem">${label}${note}</span>${input}<span class="pr-note">default ${c.default ?? '—'}</span></div>`;
    };
    k.el.querySelector('.cam-ctls').innerHTML = ctl.length ? ctl.map(row).join('') + `<div class="pr-row"><button class="btn btn-sm" onclick="camCtlReset('${esc(id)}')">Reset to defaults</button><span class="pr-note">Your values are saved and re-applied when the camera is plugged in again.</span></div>` : '<div class="pr-note">No adjustable controls.</div>';
  }

  window.camCtl = async (id, name, value) => {
    try { const r = await papi('POST', `/api/camera/${encodeURIComponent(id)}/controls`, { [name]: Number(value) }); cards[id].detail.controls = r.controls; } catch {}
    renderControls(id);
  };
  window.camCtlReset = async id => {
    try { const r = await papi('POST', `/api/camera/${encodeURIComponent(id)}/controls/reset`); cards[id].detail.controls = r.controls; } catch {}
    renderControls(id);
  };

  function headerHtml(c) {
    const live = c.streaming ? `<span class="pr-pill on">live · ${c.viewers} viewer${c.viewers === 1 ? '' : 's'}${c.fps ? ` · ${c.fps} fps` : ''}</span>` : '<span class="pr-pill">asleep</span>';
    const sig = c.streaming && c.signal === false ? ' <span class="pr-pill warn">no signal</span>' : '';
    const shared = c.share ? ` <span class="pr-pill warn">shared :${c.sharePort}</span>` : '';
    return `<h4>${esc(c.name)} ${kindPill(c)} ${live}${sig}${shared}</h4>
      <div class="pr-note">${esc(c.dev)} · USB ${esc(c.vid)}:${esc(c.pid)} on port ${esc(c.usbPort)}${c.serial ? ` · S/N ${esc(c.serial)}` : ''} · ${fmtStream(c.stream)}</div>
      ${c.error ? `<div class="pr-note" style="color:var(--danger)">${esc(c.error)}</div>` : ''}`;
  }

  function addCard(c) {
    const el = document.createElement('div');
    el.className = 'pr-card';
    el.innerHTML = `<div class="cam-hdr"></div>
      <div class="pr-row">
        <button class="btn btn-sm btn-primary cam-livebtn" onclick="camToggleLive('${esc(c.id)}')">Live view</button>
        <button class="btn btn-sm" onclick="camSnapshot('${esc(c.id)}')">Snapshot</button>
        <a class="btn btn-sm" href="/api/camera/${encodeURIComponent(c.id)}/snapshot?download=1">Download JPEG</a>
      </div>
      <div class="cam-view"></div>
      <div class="cam-snap"></div>
      <details class="cam-det"><summary class="pr-note" style="cursor:pointer;margin-top:8px">Stream settings and image controls</summary>
        <div class="cam-settings"></div><div class="cam-ctls" style="margin-top:8px"></div>
      </details>`;
    root.querySelector('.cam-list').appendChild(el);
    const k = cards[c.id] = { el, visible: false, sig: '' };
    el.querySelector('.cam-det').addEventListener('toggle', e => { if (e.target.open) loadDetail(c.id); });
    // Stop pulling frames while the card is scrolled out of view.
    k.io = new IntersectionObserver(es => { k.visible = es[0].isIntersecting; setLive(c.id); });
    k.io.observe(el);
  }

  function onVis() { for (const id in cards) setLive(id); }

  periphRegister('camera', {
    mount(el) {
      root = el;
      el.innerHTML = '<div class="cam-list"></div><div class="cam-csi"></div>';
      document.addEventListener('visibilitychange', onVis);
    },
    update(st) {
      if (!root) return;
      const ids = new Set(st.cameras.map(c => c.id));
      for (const id of Object.keys(cards)) if (!ids.has(id)) { cards[id].io.disconnect(); cards[id].el.remove(); delete cards[id]; }
      for (const c of st.cameras) {
        if (!cards[c.id]) addCard(c);
        const k = cards[c.id];
        const sig = JSON.stringify(c);
        if (k.sig !== sig) {
          k.sig = sig;
          k.el.querySelector('.cam-hdr').innerHTML = headerHtml(c);
          // The stop button only makes sense while running; rebuild that one slot cheaply.
          const row = k.el.querySelector('.pr-row');
          const stop = row.querySelector('.cam-stop');
          if (c.streaming && !stop) row.insertAdjacentHTML('beforeend', `<button class="btn btn-sm cam-stop" onclick="papi('POST','/api/camera/${encodeURIComponent(c.id)}/stop')">Stop camera</button>`);
          if (!c.streaming && stop) stop.remove();
        }
        syncButtons(c.id);
      }
      root.querySelector('.cam-list').style.display = st.cameras.length ? '' : 'none';
      root.querySelector('.cam-csi').innerHTML = (!st.cameras.length && !st.csi.length ? '<div class="pr-note">No camera connected. Plug in a USB webcam or HDMI capture stick.</div>' : '')
        + (st.csi.length ? `<div class="pr-card"><h4>CSI cameras <small>libcamera</small></h4>${st.csi.map(x => `<div class="pr-note">#${x.index} ${esc(x.sensor)} · ${esc(x.detail)}</div>`).join('')}<div class="pr-note">Listed only; streaming CSI cameras isn't supported yet.</div></div>` : '');
    },
    unmount() {
      document.removeEventListener('visibilitychange', onVis);
      for (const id in cards) { cards[id].io.disconnect(); const img = cards[id].el.querySelector('img.cam-live'); if (img) img.src = ''; if (cards[id].snapUrl) URL.revokeObjectURL(cards[id].snapUrl); delete cards[id]; }
      root = null;
    },
  });
})();
