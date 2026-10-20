import { create } from "zustand";

import type { AiImage, RdpInput } from "./ipc";

/** A picture of the remote desktop, and how its pixels map to the desktop's. */
export interface RdpScreenshot {
  image: AiImage;
  /** Size of the picture. */
  width: number;
  height: number;
  /** Desktop pixels per picture pixel (1 when the picture was not scaled down). */
  scale: number;
}

/**
 * Open remote desktop sessions by tab id, so controls outside the desktop (the tab bar, the AI
 * assistant) can act on them without covering any part of the remote screen.
 */
export interface RdpHandle {
  sendCtrlAltDel: () => void;
  /** Connects again (for example after changing the shared folder). */
  reconnect: () => void;
  /** Touch screens: shows the device keyboard. Call it from the tap itself (see RdpKeyboard). */
  openKeyboard: () => void;
  closeKeyboard: () => void;
  /** The desktop as it is on screen now, at most `maxEdge` pixels on its longer side. */
  screenshot: (maxEdge: number) => RdpScreenshot | null;
  /** Sends input as if the user did it (the AI agent, after approval). */
  input: (ops: RdpInput[]) => void;
}

const sessions = new Map<string, RdpHandle>();

export function registerRdp(tabId: string, handle: RdpHandle): () => void {
  sessions.set(tabId, handle);
  return () => {
    if (sessions.get(tabId) === handle) sessions.delete(tabId);
  };
}

export const getRdp = (tabId: string | null | undefined): RdpHandle | undefined => (tabId ? sessions.get(tabId) : undefined);

/** The sound of each remote desktop tab: unknown until the server sends some. */
export type RdpAudio = { state: "waiting" } | { state: "playing" } | { state: "failed"; error: string };

export const useRdpAudio = create<{ byTab: Record<string, RdpAudio>; set: (tabId: string, audio: RdpAudio) => void }>((set) => ({
  byTab: {},
  set: (tabId, audio) => set((s) => ({ byTab: { ...s.byTab, [tabId]: audio } })),
}));
