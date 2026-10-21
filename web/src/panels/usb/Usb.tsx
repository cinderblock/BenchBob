// USB panel: device tree (controllers → buses → hubs → devices) with per-port power chips,
// USB/IP and usbredir sharing, and the hardware timeline. Server: backend/crates/usb.

import { useState, type ReactNode } from "react";
import registry from "@registry";
import { useBob, type BobApi } from "../../bob/context";
import type { PanelProps } from "../../registry";
import { ActionLine, StatusLine, errText, useAction, type ActionStatus } from "../../ui/status";
import type { DmesgLine, PowerArgs, PowerPlan, UsbDev, UsbState } from "./api";
import "./style.css";

const PANELS: Record<string, string> = { serial: "Serial", can: "CAN", camera: "Camera", drives: "Drives", netdiag: "Network", wifi: "Wi-Fi", i2c: "I²C", gpio: "GPIO", ble: "Bluetooth", sdr: "SDR", gps: "GPS", probes: "Debug probes", ups: "UPS" };
const devName = (d: UsbDev) => [d.vendor, d.product].filter(Boolean).join(" ") || d.vidpid;
const Pill = ({ t, cls = "" }: { t: ReactNode; cls?: string }) => <span className={`pr-pill ${cls}`}>{t}</span>;

interface Sel {
  hub: string;
  port: number;
}

/** The last power action on one target (a hub, or a hub port), shown where it was switched. */
interface PowerStatus extends ActionStatus {
  action: PowerArgs["action"];
}

// Keyed by hub location, plus ":port" for a single port.
const powerKey = (hub: string, port?: number | null) => (port == null ? hub : `${hub}:${port}`);

export default function Usb({ st }: PanelProps<UsbState>) {
  const b = useBob();
  const [sel, setSel] = useState<Sel | null>(null);
  const [open, setOpen] = useState<Set<string>>(new Set());
  const [tlKind, setTlKind] = useState<"all" | "usb" | "kernel">("all");
  const [hist, setHist] = useState<DmesgLine[] | null>(null);
  const [power, setPower] = useState<Record<string, PowerStatus>>({});
  if (!st?.devices) return null;

  // The name this dashboard reached the bob by is the one that resolves from the client's
  // side; fall back to the server's own name when it was a bare IP.
  const h = b.conn.hostname;
  const clientHost = !h || /^[\d.]+$|:/.test(h) ? st.host : h;
  const toggleMore = (busid: string) =>
    setOpen((o) => {
      const n = new Set(o);
      n.has(busid) ? n.delete(busid) : n.add(busid);
      return n;
    });
  const setPowerStatus = (k: string, s: PowerStatus | null) =>
    setPower((m) => {
      const n = { ...m };
      if (s) n[k] = s;
      else delete n[k];
      return n;
    });
  const ctx: RowCtx = { st, sel, setSel, open, toggleMore, clientHost, power, setPowerStatus };

  const shown = new Set<string>();
  const tree: ReactNode[] = [];
  for (const c of st.controllers) {
    tree.push(
      <div className="usb-ctl" key={"c" + c.id}>
        {c.name} · {c.driver} · {c.id}
      </div>,
    );
    for (const d of st.devices) {
      if (!c.buses.includes(d.bus)) continue;
      shown.add(d.busid);
      tree.push(<DevRow key={d.busid} d={d} ctx={ctx} />);
    }
  }
  const rest = st.devices.filter((d) => !shown.has(d.busid));
  if (rest.length)
    tree.push(
      <div className="usb-ctl" key="other">
        Other
      </div>,
      ...rest.map((d) => <DevRow key={d.busid} d={d} ctx={ctx} />),
    );

  return (
    <>
      <div className="usb-tree">
        {tree}
        {!st.uhubctl.available ? (
          <div className="pr-note">
            Port power switching needs <span className="pr-endpoint">uhubctl</span> (apt: uhubctl).
          </div>
        ) : st.uhubctl.error ? (
          <div className="pr-note">uhubctl: {st.uhubctl.error}</div>
        ) : null}
      </div>
      <div className="pr-card">
        <Share st={st} host={clientHost} />
      </div>
      <div className="pr-card">
        <Timeline st={st} kind={tlKind} setKind={setTlKind} hist={hist} setHist={setHist} />
      </div>
    </>
  );
}

interface RowCtx {
  st: UsbState;
  sel: Sel | null;
  setSel(s: Sel | null): void;
  open: Set<string>;
  toggleMore(busid: string): void;
  clientHost: string;
  power: Record<string, PowerStatus>;
  setPowerStatus(k: string, s: PowerStatus | null): void;
}

function PanelLink({ p, dev = "" }: { p?: string; dev?: string }) {
  const b = useBob();
  if (!p) return null;
  const name = PANELS[p] || p;
  if (p !== "drives" && !registry.panels[p]) return <> → {name}</>;
  return (
    <>
      {" "}
      →{" "}
      <button
        className="usb-link"
        onClick={() => {
          if (p !== "drives") b.showPanel(p);
          else {
            const n = dev.replace("/dev/", "");
            if (!b.scrollToDrive(n)) document.querySelector(".bob [data-drive]")?.scrollIntoView({ behavior: "smooth" });
          }
        }}
      >
        {name} panel
      </button>
    </>
  );
}

const POWER_BUSY = { on: "switching on", off: "switching off", cycle: "power cycling" };
const POWER_DONE = { on: "Switched on", off: "Switched off", cycle: "Power cycled" };

// Progress and failures go to `report`, which shows them beside the buttons that did it.
async function doPower(b: BobApi, args: PowerArgs, report: (s: PowerStatus) => void) {
  const status = (state: PowerStatus["state"], msg: string) => report({ action: args.action, state, msg, t: Date.now() });
  const fail = (e: unknown) => status("error", `Power ${args.action} failed: ${errText(e)}`);
  let plan: PowerPlan;
  try {
    plan = await b.api<PowerPlan>("GET", "/api/usb/power/plan?" + new URLSearchParams(Object.entries(args).map(([k, v]) => [k, String(v)])));
  } catch (e) {
    return fail(e);
  }
  if (plan.needs !== "none" || (args.action !== "on" && plan.devices.length)) {
    const level = await confirmPower(b, plan);
    if (!level) return;
    if (plan.needs !== "none") args.confirm = plan.needs;
  }
  status("busy", POWER_BUSY[args.action] + "…");
  try {
    await b.api("POST", "/api/usb/power", args);
    status("ok", POWER_DONE[args.action]);
  } catch (e) {
    fail(e);
  }
}

function confirmPower(b: BobApi, plan: PowerPlan): Promise<string | null> {
  return new Promise((resolve) => b.openModal(<PowerConfirm plan={plan} resolve={resolve} />, { onDismiss: () => resolve(null) }));
}

function PowerConfirm({ plan, resolve }: { plan: PowerPlan; resolve(v: string | null): void }) {
  const b = useBob();
  const unsafe = plan.needs === "unsafe";
  const word = "CUT POWER";
  const [text, setText] = useState("");
  const done = (v: string | null) => {
    b.closeModal();
    resolve(v);
  };
  return (
    <>
      <h3 style={unsafe ? { color: "var(--danger)" } : undefined}>Switch port power {plan.action}?</h3>
      {plan.warnings.map((w) => (
        <p key={w}>{w}</p>
      ))}
      <div className="modal-dev">
        Ports: {plan.ports.join(", ")}
        <br />
        Devices losing power: {plan.devices.join(", ") || "none"}
      </div>
      {unsafe && (
        <div className="confirm-box">
          <label htmlFor="usb-cw">Type {word} to confirm:</label>
          <input type="text" id="usb-cw" autoComplete="off" value={text} onInput={(e) => setText(e.currentTarget.value)} />
        </div>
      )}
      <div className="modal-btns">
        <button className="btn" onClick={() => done(null)}>
          Cancel
        </button>
        <button className={`btn ${unsafe ? "btn-danger" : "btn-warn"}`} disabled={unsafe && text !== word} onClick={() => done(plan.needs)}>
          {plan.group ? `Switch all ${plan.ports.length} ports` : "Switch"}
        </button>
      </div>
    </>
  );
}

// `pk` is the powerKey of the hub or port these buttons switch (for a busid target, the port
// it's plugged into), so the status shows wherever that port's buttons are.
function PowerBtns({ t, pk, ctx }: { t: { hub?: string; port?: number; busid?: string }; pk: string; ctx: RowCtx }) {
  const b = useBob();
  const [delay, setDelay] = useState("2");
  const s = ctx.power[pk];
  const busy = s?.state === "busy";
  const go = (action: PowerArgs["action"]) => {
    const args: PowerArgs = { action };
    if (t.busid) args.busid = t.busid;
    else {
      args.hub = t.hub;
      if (t.port != null) args.port = t.port;
    }
    args.delay = +delay || 2;
    doPower(b, args, (st) => ctx.setPowerStatus(pk, st));
  };
  return (
    <>
      <button className="btn btn-sm" disabled={busy} onClick={() => go("on")}>
        On
      </button>
      <button className="btn btn-sm btn-warn" disabled={busy} onClick={() => go("off")}>
        Off
      </button>
      <button className="btn btn-sm" disabled={busy} onClick={() => go("cycle")}>
        Cycle
      </button>
      <label className="pr-note">
        off for <input type="number" min={1} max={30} value={delay} onInput={(e) => setDelay(e.currentTarget.value)} /> s
      </label>
      <StatusLine s={s} onClear={() => ctx.setPowerStatus(pk, null)} />
    </>
  );
}

function PortChips({ d, ctx }: { d: UsbDev; ctx: RowCtx }) {
  const pw = d.power;
  if (!pw) return null;
  if (!pw.switchable || !pw.ports) {
    const used = d.ports.filter((p) => p.child).length;
    return (
      <div className="usb-l2">
        {d.nports} port{d.nports === 1 ? "" : "s"}, {used} in use · {pw.why || "power not switchable"}
      </div>
    );
  }
  const grouped = !!pw.group;
  const { sel } = ctx;
  let bar: ReactNode = null;
  if (grouped && pw.group!.hubs[0] === pw.loc)
    bar = (
      <div className="usb-bar">
        <span>⚠ {pw.group!.label}.</span>
        <PowerBtns t={{ hub: pw.loc }} pk={powerKey(pw.loc)} ctx={ctx} />
      </div>
    );
  else if (grouped) bar = <div className="usb-l2">Ganged with hub {pw.group!.hubs[0]}: same sockets, switched there.</div>;
  else if (sel && sel.hub === pw.loc) {
    const p = pw.ports.find((x) => x.port === sel.port);
    if (p)
      bar = (
        <div className="usb-bar">
          <b>
            Hub {pw.loc} port {p.port}
          </b>
          <PowerBtns key={`${pw.loc}:${p.port}`} t={{ hub: pw.loc, port: p.port }} pk={powerKey(pw.loc, p.port)} ctx={ctx} />
        </div>
      );
  }
  return (
    <>
      <div className="usb-ports">
        {pw.ports.map((p) => {
          const child = d.ports.find((x) => x.port === p.port)?.child;
          const oc = p.overCurrent || d.ports.find((x) => x.port === p.port)?.overCurrent;
          const cls = p.internal ? "int" : oc ? "oc" : p.power ? "on" : "off";
          const state = p.internal ? "internal" : oc ? "over-current" : p.power ? "on" : "off";
          const isSel = sel && sel.hub === pw.loc && sel.port === p.port;
          // The port's own status, else a whole-hub switch (a ganged one is made on the group's first hub).
          const ps = ctx.power[powerKey(pw.loc, p.port)] || ctx.power[powerKey(grouped ? pw.group!.hubs[0] : pw.loc)];
          const mark = ps?.state === "busy" ? " busy" : ps?.state === "error" ? " err" : "";
          return (
            <button key={p.port} className={`usb-chip ${cls}${isSel ? " sel" : ""}${mark}`} disabled={!!p.internal || grouped} onClick={() => ctx.setSel(isSel ? null : { hub: pw.loc, port: p.port })}>
              P{p.port} {ps?.state === "busy" ? ps.msg : state}
              {child ? " · used" : ""}
            </button>
          );
        })}
      </div>
      {bar}
    </>
  );
}

function DevRow({ d, ctx }: { d: UsbDev; ctx: RowCtx }) {
  const b = useBob();
  // Share/Unshare swap places when they work, so their status belongs to the row.
  const share = useAction();
  const depth = Math.min(d.depth, 6);
  const style = depth ? { marginLeft: depth * 12 } : undefined;
  if (d.root) {
    const ver = d.speed >= 5000 ? "USB 3" : "USB 2";
    return (
      <div className="usb-dev root" style={style}>
        <div className="usb-l1">
          <b>Bus {d.bus}</b>
          <span className="pr-note">
            {ver} root hub · {d.speedLabel} · {d.nports} port{d.nports === 1 ? "" : "s"}
          </span>
          {d.power?.mode && <Pill t={d.power.group ? "ganged" : d.power.mode} />}
        </div>
        <PortChips d={d} ctx={ctx} />
      </div>
    );
  }
  const meta = [d.vidpid, d.version && `USB ${d.version}`, d.maxPowerMa ? `${d.maxPowerMa} mA` : "", d.config ? `cfg ${d.config}` : "", d.hub ? `${d.nports} ports` : ""].filter(Boolean).join(" · ");
  const isOpen = ctx.open.has(d.busid);
  return (
    <div className="usb-dev" style={style}>
      <div className="usb-l1">
        <span className="usb-pn">{d.busid}</span>
        <b>{devName(d)}</b>
        <span className="pr-note">{meta}</span>
        {d.speedLabel && <Pill t={d.speedLabel} />}
        {d.usbip && <Pill t={d.usbip === "in use" ? "USB/IP: in use by remote" : d.usbip === "error" ? "USB/IP: error" : "USB/IP: shared, available"} cls={d.usbip === "error" ? "bad" : "on"} />}
        {d.redir && <Pill t={`usbredir :${d.redir.port}${d.redir.peers.length ? " · " + d.redir.peers.join(", ") : ""}`} cls="on" />}
        {d.protect.length > 0 && <Pill t="protected" cls="warn" />}
        {d.hub && d.power?.mode && <Pill t={d.power.group ? "ganged" : d.power.switchable ? d.power.mode : "no switching"} />}
        {!d.hub && (
          <>
            {d.usbip ? (
              <button className="btn btn-sm" disabled={share.busy} onClick={() => share.run(() => b.api("POST", "/api/usb/unshare", { busid: d.busid }), { busy: "Unsharing…", ok: "Stopped sharing" })}>
                Unshare
              </button>
            ) : !d.redir && !d.shareBlock ? (
              <button className="btn btn-sm" disabled={share.busy} onClick={() => share.run(() => b.api<{ attach: string }>("POST", "/api/usb/share", { busid: d.busid }), { busy: "Sharing…", ok: (r) => `Shared. Attach with: ${r.attach}` })}>
                Share (USB/IP)
              </button>
            ) : null}
            <button className="btn btn-sm" onClick={() => ctx.toggleMore(d.busid)}>
              {isOpen ? "Less" : "More"}
            </button>
            <ActionLine a={share} />
          </>
        )}
      </div>
      {!d.hub && (
        <>
          {d.interfaces.length > 0 && (
            <div className="usb-l2">
              {d.interfaces.map((i, k) => (
                <span key={k}>
                  {k > 0 && " · "}
                  {i.clsName} → {i.driver ? i.driver : <span className="nd">no driver</span>}
                </span>
              ))}
            </div>
          )}
          {d.nodes.length > 0 && (
            <div className="usb-l2">
              {d.nodes.map((n, k) => (
                <span key={n.dev}>
                  {k > 0 && " · "}
                  {n.dev}
                  {n.label ? ` (${n.label})` : ""}
                  <PanelLink p={n.panel} dev={n.dev} />
                </span>
              ))}
            </div>
          )}
          {d.hint && (
            <div className="usb-l2">
              {d.hint.label}
              <PanelLink p={d.hint.panel} />
            </div>
          )}
          {d.usbip && <div className="usb-l2">Detached from local drivers while shared</div>}
        </>
      )}
      {d.protect.length > 0 && <div className="usb-l2">Protected: this device {d.protect.join("; ")}</div>}
      {d.hub && <PortChips d={d} ctx={ctx} />}
      {isOpen && <More d={d} ctx={ctx} />}
    </div>
  );
}

function More({ d, ctx }: { d: UsbDev; ctx: RowCtx }) {
  const b = useBob();
  const rules = useAction();
  const redir = useAction();
  const st = ctx.st;
  const vp = d.rules.includes("vidpid"),
    pt = d.rules.includes("port");
  const rule = (match: string, value: string, on: boolean) => rules.run(() => b.api(on ? "POST" : "DELETE", "/api/usb/rules", { match, value }), { busy: "Saving…" });
  return (
    <div className="usb-more">
      <div>
        {d.serial && (
          <>
            Serial <span className="pr-endpoint">{d.serial}</span> ·{" "}
          </>
        )}
        Class {d.cls} · driver {d.driver} · device {d.bus}-{d.devnum}
        {d.idPath && (
          <>
            {" "}
            · <span className="pr-endpoint">{d.idPath}</span>
          </>
        )}
      </div>
      <div className="usb-cmd">
        Auto-share:{" "}
        <label>
          <input type="checkbox" checked={vp} onChange={(e) => rule("vidpid", d.vidpid, e.currentTarget.checked)} /> every {d.vidpid}
        </label>
        <label>
          <input type="checkbox" checked={pt} onChange={(e) => rule("port", d.busid, e.currentTarget.checked)} /> anything on port {d.busid}
        </label>
        <ActionLine a={rules} />
      </div>
      {d.shareBlock && !d.usbip && <div className="pr-note">{d.shareBlock}</div>}
      {st.redir.available ? (
        d.redir ? (
          <>
            <div className="usb-cmd">
              usbredir on TCP {d.redir.port}{" "}
              <button className="btn btn-sm" disabled={redir.busy} onClick={() => redir.run(() => b.api("POST", "/api/usb/redir/stop", { busid: d.busid }), { busy: "Stopping…" })}>
                Stop usbredir
              </button>
              <ActionLine a={redir} />
            </div>
            <div className="usb-cmd">
              QEMU{" "}
              <span className="pr-endpoint">
                -chardev socket,id=redir0,host={ctx.clientHost},port={d.redir.port} -device usb-redir,chardev=redir0
              </span>
            </div>
          </>
        ) : !d.usbip && !d.shareBlock ? (
          <div className="usb-cmd">
            <button className="btn btn-sm" disabled={redir.busy} onClick={() => redir.run(() => b.api("POST", "/api/usb/redir/start", { busid: d.busid }), { busy: "Starting…" })}>
              Serve with usbredir
            </button>
            <span className="pr-note">for QEMU/libvirt usb-redir; exclusive with USB/IP</span>
            <ActionLine a={redir} />
          </div>
        ) : null
      ) : (
        <div className="pr-note">
          usbredir unavailable: install the <span className="pr-endpoint">{st.redir.apt}</span> package
        </div>
      )}
      {d.upstream && (
        <div className="usb-cmd">
          Port power (hub {d.upstream.hub} port {d.upstream.port}): <PowerBtns t={{ busid: d.busid }} pk={powerKey(d.upstream.hub, d.upstream.port)} ctx={ctx} />
        </div>
      )}
    </div>
  );
}

function Share({ st, host }: { st: UsbState; host: string }) {
  const b = useBob();
  const rules = useAction();
  const save = useAction();
  const u = st.usbip;
  // null = untouched, so the field follows the server until someone edits it.
  const [usbipPort, setUsbipPort] = useState<string | null>(null);
  const [redirPort, setRedirPort] = useState<string | null>(null);
  const shared = st.devices.filter((d) => d.usbip);
  const head = u.running ? `usbipd on TCP ${u.port}${u.peers.length ? " · clients " + u.peers.join(", ") : " · no clients"}` : u.available ? `usbipd starts on TCP ${u.port} when a device is shared` : "usbip is not installed (apt: usbip)";
  const pf = u.port === 3240 ? "" : ` --tcp-port ${u.port}`;
  const bid = shared[0]?.busid || "<busid>";
  return (
    <>
      <h4>
        Network sharing <small>{head}</small>
      </h4>
      {u.error && (
        <div className="pr-note" style={{ color: "var(--danger)" }}>
          {u.error}
        </div>
      )}
      <div className="pr-note">Sharing detaches the device from BenchBob: it disappears from the Serial, CAN, Drives and other panels until you unshare it.</div>
      <div className="usb-cmd pr-note">
        Linux client:{" "}
        <span className="pr-endpoint">
          usbip{pf} list -r {host}
        </span>{" "}
        <span className="pr-endpoint">
          sudo usbip{pf} attach -r {host} -b {bid}
        </span>
      </div>
      <div className="usb-cmd pr-note">
        Windows: install usbip-win2, then{" "}
        <span className="pr-endpoint">
          usbip.exe{pf} attach -r {host} -b {bid}
        </span>
      </div>
      <div className="usb-cmd pr-note">
        usbredir:{" "}
        {st.redir.available ? (
          `available; sessions start at TCP ${st.redir.basePort}`
        ) : (
          <>
            not installed (apt: <span className="pr-endpoint">{st.redir.apt}</span>)
          </>
        )}
      </div>
      <div className="usb-cmd pr-note">
        Auto-share rules:{" "}
        {st.rules.length ? (
          st.rules.map((r) => (
            <span key={r.match + r.value} className="pr-pill">
              {r.match === "port" ? "port " : ""}
              {r.value}{" "}
              <button className="pr-x" aria-label="Remove rule" onClick={() => rules.run(() => b.api("DELETE", "/api/usb/rules", { match: r.match, value: r.value }), { busy: "Removing…" })}>
                &times;
              </button>
            </span>
          ))
        ) : (
          <span className="pr-note">none</span>
        )}
        <ActionLine a={rules} />
      </div>
      <div className="pr-row">
        <label>
          usbipd port <input type="number" value={usbipPort ?? String(u.port)} onInput={(e) => setUsbipPort(e.currentTarget.value)} style={{ width: "6em" }} />
        </label>
        <label>
          usbredir first port <input type="number" value={redirPort ?? String(st.redir.basePort)} onInput={(e) => setRedirPort(e.currentTarget.value)} style={{ width: "6em" }} />
        </label>
        <button
          className="btn btn-sm"
          disabled={save.busy}
          onClick={async () => {
            if (await save.run(() => b.api("POST", "/api/usb/config", { usbipPort: +(usbipPort ?? u.port), redirBasePort: +(redirPort ?? st.redir.basePort) }), { busy: "Saving…", ok: "Ports saved" })) {
              setUsbipPort(null);
              setRedirPort(null);
            }
          }}
        >
          Save ports
        </button>
        <ActionLine a={save} />
      </div>
    </>
  );
}

function Timeline({ st, kind, setKind, hist, setHist }: { st: UsbState; kind: "all" | "usb" | "kernel"; setKind(k: "all" | "usb" | "kernel"): void; hist: DmesgLine[] | null; setHist(h: DmesgLine[] | null): void }) {
  const b = useBob();
  const load = useAction();
  const kinds ={ all: null, usb: ["plug", "unplug", "power", "share"], kernel: ["kernel"] }[kind];
  const rows = (st.timeline || []).filter((e) => !kinds || kinds.includes(e.detail?.kind || ""));
  const fmt = (t: number) => new Date(t).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit", second: "2-digit" });
  const f = (k: "all" | "usb" | "kernel", l: string) => (
    <button className={`btn btn-sm${kind === k ? " btn-primary" : ""}`} onClick={() => setKind(k)}>
      {l}
    </button>
  );
  return (
    <>
      <h4>
        Timeline <small>newest first</small>
        <span className="usb-filt">
          {f("all", "All")}
          {f("usb", "USB")}
          {f("kernel", "Kernel")}
          <button
            className="btn btn-sm"
            disabled={load.busy}
            onClick={async () => {
              if (hist) return setHist(null);
              const r = await load.run(() => b.api<{ lines: DmesgLine[] }>("GET", "/api/usb/dmesg"), { busy: "Loading…" });
              if (r) setHist(r.lines);
            }}
          >
            {hist ? "Hide boot log" : "Since boot"}
          </button>
        </span>
      </h4>
      <ActionLine a={load} />
      <div className="pr-log" style={{ maxHeight: 240 }}>
        {rows.length ? (
          rows.map((e, i) => (
            <div key={i} className={`usb-ev ${e.detail?.level || ""}`}>
              <span className="t">{fmt(e.t)}</span>
              {e.source !== "usb" ? <Pill t={e.source} /> : e.detail?.kind ? <Pill t={e.detail.kind} /> : null}
              <span>{e.text}</span>
            </div>
          ))
        ) : (
          <div className="pr-note">Nothing yet. Plug something in.</div>
        )}
        {hist && (
          <>
            <div className="pr-note" style={{ marginTop: 6 }}>
              Kernel log since boot (historical, newest first)
            </div>
            {hist
              .slice()
              .reverse()
              .map((k, i) => (
                <div key={"h" + i} className={`usb-ev ${k.level}`}>
                  <span className="t">{k.ts.slice(11, 19)}</span>
                  <span>{k.text}</span>
                </div>
              ))}
          </>
        )}
      </div>
      {st.kernelLog?.error && <div className="pr-note">{st.kernelLog.error}</div>}
    </>
  );
}
