// What every section and panel gets from the <Bob> it renders in: the connection, the
// live store, and the page-level helpers (toasts, the modal, panel navigation). All of it
// is per bob, so two <Bob>s in one page are independent.

import { createContext, useContext, useSyncExternalStore, type ReactNode } from "react";
import type { BobConnection } from "../lib/connection";
import type { BobSnapshot, BobStore } from "../lib/store";

export type ToastKind = "success" | "error" | "info";

export interface ModalOptions {
  /** Extra classes on .modal: "wide", "wide flash". */
  cls?: string;
  /** Called when the dialog is dismissed from outside (backdrop, Escape, another modal). */
  onDismiss?(): void;
}

export interface BobApi {
  conn: BobConnection;
  store: BobStore;
  /**
   * JSON request; throws on failure. Callers show the failure where the action was taken
   * (ui/status.tsx: useAction, AsyncButton), never silently.
   */
  api<T = any>(method: string, path: string, body?: unknown): Promise<T>;
  toast(msg: string, kind?: ToastKind): void;
  /** Open a dialog. `content` renders inside this bob's context, so it can use hooks. */
  openModal(content: ReactNode, opts?: ModalOptions): void;
  closeModal(): void;
  /** Title, message and a device box; resolves true on Confirm. */
  confirm(title: string, msg: string, dev?: string): Promise<boolean>;
  /** Unfold a peripheral panel and scroll to it. */
  showPanel(id: string): void;
  hidePanel(id: string): void;
  scrollToDrive(name: string): boolean;
  /** Scoped localStorage key for this bob. */
  key(k: string): string;
}

export const BobContext = createContext<BobApi | null>(null);

export function useBob(): BobApi {
  const b = useContext(BobContext);
  if (!b) throw new Error("useBob outside <Bob>");
  return b;
}

export function useSnapshot(): BobSnapshot {
  const { store } = useBob();
  return useSyncExternalStore(store.subscribe, store.get);
}
