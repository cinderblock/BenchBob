// The whole dashboard for one bob. Everything bob-specific (the status stream, panel
// sockets, the modal, toasts, which panels are open) lives under one <Bob>, so a page can
// render several side by side: a desktop app with a tab or column per bob.

import { Suspense, useCallback, useEffect, useMemo, useRef, useState, type ReactNode } from "react";
import registry from "@registry";
import { BobContext, useBob, useSnapshot, type BobApi, type ModalOptions, type ToastKind } from "./context";
import { BobConnection } from "../lib/connection";
import { BobStore } from "../lib/store";
import { readLocal, scopedKey, writeLocal } from "../lib/storage";
import { Header } from "../shell/Header";
import { PowerBanner, UpdateBanner, VersionNotice } from "../shell/Banners";
import { PeriphHost } from "../shell/Periph";
import { ErrorBoundary } from "../shell/ErrorBoundary";

export interface BobProps {
  connection: BobConnection;
  /** The build version stamped into the page by the bob serving it (`__BUILD_VERSION__`). */
  pageBuild?: string | null;
  /** Set document.title from the bob's hostname. */
  setTitle?: boolean;
  /** Microcontroller-node UI only (the MCU build). */
  nodeOnly?: boolean;
  /** For tests: a store to use instead of making one. */
  store?: BobStore;
}

interface Toast {
  id: number;
  msg: string;
  kind: ToastKind;
}

interface ModalState {
  content: ReactNode;
  opts: ModalOptions;
  key: number;
}

export function Bob({ connection, pageBuild, setTitle = true, nodeOnly = false, store: given }: BobProps) {
  const store = useMemo(() => given || new BobStore(connection, { pageBuild, nodeOnly }), [connection, given]);
  useEffect(() => {
    store.start();
    return () => store.stop();
  }, [store]);

  const [toasts, setToasts] = useState<Toast[]>([]);
  const toastId = useRef(0);
  const toast = useCallback((msg: string, kind: ToastKind = "info") => {
    const id = ++toastId.current;
    setToasts((t) => [...t, { id, msg, kind }]);
    setTimeout(() => setToasts((t) => t.filter((x) => x.id !== id)), 5000);
  }, []);

  const [modal, setModal] = useState<ModalState | null>(null);
  const modalRef = useRef<ModalState | null>(null);
  modalRef.current = modal;
  const modalKey = useRef(0);
  const openModal = useCallback((content: ReactNode, opts: ModalOptions = {}) => {
    // A dialog replacing another counts as dismissing the first.
    modalRef.current?.opts.onDismiss?.();
    setModal({ content, opts, key: ++modalKey.current });
  }, []);
  const closeModal = useCallback(() => setModal(null), []);
  const dismissModal = useCallback(() => {
    modalRef.current?.opts.onDismiss?.();
    setModal(null);
  }, []);

  const key = useCallback((k: string) => scopedKey(connection, k), [connection]);
  const [open, setOpen] = useState<Set<string>>(() => new Set(readLocal<string[]>(scopedKey(connection, "periphOpen"), [])));
  const rootRef = useRef<HTMLDivElement>(null);
  const scrollTo = useRef<string | null>(null);

  const api = useMemo<BobApi>(() => {
    const a: BobApi = {
      conn: connection,
      store,
      async api(method, path, body) {
        try {
          return await connection.json(method, path, body);
        } catch (e: any) {
          toast(e instanceof TypeError ? "Request failed: " + e.message : e.message, "error");
          throw e;
        }
      },
      toast,
      openModal,
      closeModal,
      confirm: (title, msg, dev) =>
        new Promise<boolean>((resolve) => {
          const done = (v: boolean) => {
            closeModal();
            resolve(v);
          };
          openModal(
            <>
              <h3>{title}</h3>
              <p>{msg}</p>
              {dev && <div className="modal-dev" style={{ whiteSpace: "pre-line" }}>{dev}</div>}
              <div className="modal-btns">
                <button className="btn" onClick={() => done(false)}>
                  Cancel
                </button>
                <button className="btn btn-primary" onClick={() => done(true)}>
                  Confirm
                </button>
              </div>
            </>,
            { onDismiss: () => resolve(false) },
          );
        }),
      showPanel(id) {
        setOpen((o) => {
          const n = new Set(o).add(id);
          writeLocal(key("periphOpen"), JSON.stringify([...n]));
          return n;
        });
        scrollTo.current = id;
      },
      hidePanel(id) {
        setOpen((o) => {
          const n = new Set(o);
          n.delete(id);
          writeLocal(key("periphOpen"), JSON.stringify([...n]));
          return n;
        });
      },
      scrollToDrive(name) {
        const el = rootRef.current?.querySelector<HTMLElement>(`[data-drive="${CSS.escape(name)}"]`);
        if (!el) return false;
        el.scrollIntoView({ behavior: "smooth", block: "center" });
        el.classList.remove("highlight");
        void el.offsetWidth;
        el.classList.add("highlight");
        return true;
      },
      key,
    };
    return a;
  }, [connection, store, toast, openModal, closeModal, key]);

  // Scroll to a panel the user just unfolded, once it has rendered.
  useEffect(() => {
    if (!scrollTo.current) return;
    const el = rootRef.current?.querySelector<HTMLElement>(`[data-panel="${CSS.escape(scrollTo.current)}"]`);
    if (el) {
      el.scrollIntoView({ behavior: "smooth", block: "start" });
      scrollTo.current = null;
    }
  });

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape" && modalRef.current) dismissModal();
    };
    addEventListener("keydown", onKey);
    return () => removeEventListener("keydown", onKey);
  }, [dismissModal]);

  return (
    <BobContext.Provider value={api}>
      <div className="bob" ref={rootRef}>
        <Dashboard open={open} setTitle={setTitle} modalOpen={!!modal} />
        {modal && (
          <div
            className="modal-bg"
            onClick={(e) => {
              if (e.target === e.currentTarget) dismissModal();
            }}
          >
            <div className={"modal" + (modal.opts.cls ? " " + modal.opts.cls : "")} role="dialog" aria-modal="true" key={modal.key}>
              <ErrorBoundary name="dialog">{modal.content}</ErrorBoundary>
            </div>
          </div>
        )}
        <div className="toasts">
          {toasts.map((t) => (
            <div key={t.id} className={`toast toast-${t.kind}`} role="status">
              {t.msg}
            </div>
          ))}
        </div>
      </div>
    </BobContext.Provider>
  );
}

function Dashboard({ open, setTitle, modalOpen }: { open: Set<string>; setTitle: boolean; modalOpen: boolean }) {
  const snap = useSnapshot();
  const { toast } = useBob();
  const s = snap.status;

  useEffect(() => {
    if (!setTitle) return;
    const name = s?.hostname || snap.node?.hostname;
    if (name) document.title = name;
  }, [setTitle, s?.hostname, snap.node?.hostname]);

  // The dashboard is left open for hours, so a deploy would otherwise leave a stale page
  // talking to a newer backend. Reload, but not out from under a dialog being filled in.
  const reloading = useRef(false);
  useEffect(() => {
    if (!snap.reloadPending || modalOpen || reloading.current) return;
    reloading.current = true;
    toast("New dashboard version — reloading", "info");
    setTimeout(() => location.reload(), 1500);
  }, [snap.reloadPending, modalOpen]);

  const L = registry.linux;
  const N = registry.node;
  const nodeEndpoints = snap.nodeInfo?.endpoints || {};
  return (
    <>
      <Header />
      <main>
        {snap.kind === "linux" && s && L && (
          <>
            <PowerBanner p={s.power} />
            <UpdateBanner />
            <VersionNotice />
            <Lazy name="system">
              <L.system status={s} />
            </Lazy>
            <Lazy name="boot">
              <L.boot status={s} />
            </Lazy>
            <Lazy name="drives">
              <L.drives status={s} />
            </Lazy>
            <PeriphHost registry={registry} status={s} open={open} />
          </>
        )}
        {snap.kind === "linux" && !L && (
          <div className="section">
            <div className="empty">
              This is the microcontroller-node build of the BenchBob UI, and this bob is a full BenchBob server.
              <div className="empty-sub">Open the server's own page instead.</div>
            </div>
          </div>
        )}
        {snap.kind === "node" && (
          <>
            <VersionNotice />
            <Lazy name="node-status">
              <N.status />
            </Lazy>
            {"GET /api/gpio" in nodeEndpoints && (
              <Lazy name="node-gpio">
                <N.gpio />
              </Lazy>
            )}
            {"GET /api/i2c" in nodeEndpoints && (
              <Lazy name="node-i2c">
                <N.i2c />
              </Lazy>
            )}
          </>
        )}
        {snap.kind === "connecting" && (
          <div className="section">
            <div className="empty">{snap.connected ? "Loading…" : "Connecting..."}</div>
          </div>
        )}
      </main>
    </>
  );
}

function Lazy({ name, children }: { name: string; children: ReactNode }) {
  return (
    <ErrorBoundary name={name}>
      <Suspense fallback={null}>{children}</Suspense>
    </ErrorBoundary>
  );
}
