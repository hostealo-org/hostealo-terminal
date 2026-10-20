// The AI agent's hands on a remote desktop: it answers with one ```action block holding a JSON
// action (see AGENT_DESKTOP_PROMPT in src-tauri/src/ai/mod.rs); after the user approves it, this
// turns it into mouse and keyboard input, and the next screenshot shows what happened.

import { isDangerousCommand } from "./danger";
import type { RdpInput } from "./ipc";
import { getRdp, type RdpScreenshot } from "./rdpSessions";
import { SCANCODES } from "./scancodes";

export type DesktopAction =
  | { action: "click"; x: number; y: number; button?: "left" | "right" | "middle"; clicks?: number }
  | { action: "move"; x: number; y: number }
  | { action: "drag"; x: number; y: number; toX: number; toY: number }
  | { action: "scroll"; x: number; y: number; amount: number }
  | { action: "type"; text: string }
  | { action: "key"; keys: string }
  | { action: "wait"; seconds: number }
  | { action: "screenshot" };

/** Longest side of the screenshots sent to the model (within every vision model's limits). */
export const SCREENSHOT_EDGE = 1280;
/** Time for the desktop to react before the next screenshot. */
const SETTLE_MS = 900;
const MAX_WAIT_SECS = 10;

/** The last screenshot sent for each tab: the model's coordinates are in its pixels. */
const lastShots = new Map<string, RdpScreenshot>();

export function takeScreenshot(tabId: string): RdpScreenshot | null {
  const shot = getRdp(tabId)?.screenshot(SCREENSHOT_EDGE) ?? null;
  if (shot) lastShots.set(tabId, shot);
  return shot;
}

/** The action the assistant asked for: the last complete ```action block of its reply. */
export function actionBlockIn(reply: string): { raw: string; index: number } | null {
  const blocks = [...reply.matchAll(/```action[^\n]*\n([\s\S]*?)```/g)];
  const last = blocks.at(-1);
  return last ? { raw: last[1].trim(), index: last.index ?? 0 } : null;
}

const isNumber = (value: unknown): value is number => typeof value === "number" && Number.isFinite(value);

/** Parses and checks an action; `null` when it is not one this agent can do. */
export function parseAction(raw: string): DesktopAction | null {
  let value: unknown;
  try {
    value = JSON.parse(raw);
  } catch {
    return null;
  }
  if (typeof value !== "object" || value === null) return null;
  const a = value as Record<string, unknown>;
  switch (a.action) {
    case "click":
      if (!isNumber(a.x) || !isNumber(a.y)) return null;
      if (a.button !== undefined && !["left", "right", "middle"].includes(a.button as string)) return null;
      return { action: "click", x: a.x, y: a.y, button: (a.button as "left" | "right" | "middle") ?? "left", clicks: a.clicks === 2 ? 2 : 1 };
    case "move":
      return isNumber(a.x) && isNumber(a.y) ? { action: "move", x: a.x, y: a.y } : null;
    case "drag":
      return isNumber(a.x) && isNumber(a.y) && isNumber(a.toX) && isNumber(a.toY) ? { action: "drag", x: a.x, y: a.y, toX: a.toX, toY: a.toY } : null;
    case "scroll":
      return isNumber(a.x) && isNumber(a.y) && isNumber(a.amount) ? { action: "scroll", x: a.x, y: a.y, amount: Math.trunc(a.amount) } : null;
    case "type":
      return typeof a.text === "string" && a.text !== "" ? { action: "type", text: a.text } : null;
    case "key":
      return typeof a.keys === "string" && keyCodes(a.keys) !== null ? { action: "key", keys: a.keys } : null;
    case "wait":
      return { action: "wait", seconds: isNumber(a.seconds) ? Math.min(MAX_WAIT_SECS, Math.max(0, a.seconds)) : 2 };
    case "screenshot":
      return { action: "screenshot" };
    default:
      return null;
  }
}

/** A short, language-neutral description for the approval card. */
export function describeAction(action: DesktopAction): string {
  switch (action.action) {
    case "click":
      return `${action.clicks === 2 ? "double-click" : action.button === "right" ? "right-click" : action.button === "middle" ? "middle-click" : "click"} (${action.x}, ${action.y})`;
    case "move":
      return `move (${action.x}, ${action.y})`;
    case "drag":
      return `drag (${action.x}, ${action.y}) → (${action.toX}, ${action.toY})`;
    case "scroll":
      return `scroll ${action.amount > 0 ? "down" : "up"} ${Math.abs(action.amount)} (${action.x}, ${action.y})`;
    case "type":
      return `type "${action.text}"`;
    case "key":
      return `key ${action.keys}`;
    case "wait":
      return `wait ${action.seconds}s`;
    case "screenshot":
      return "screenshot";
  }
}

/** Typing a destructive command, or keys that close or sign out, always need a click. */
export function isRiskyAction(action: DesktopAction): boolean {
  if (action.action === "type") return isDangerousCommand(action.text);
  if (action.action === "key") return /\b(alt\+f4|ctrl\+alt\+(del|delete|end))\b/i.test(action.keys.replace(/\s/g, ""));
  return false;
}

/** Actions that only look: run without asking even when auto-approve is off. */
export const isPassiveAction = (action: DesktopAction) => action.action === "screenshot" || action.action === "wait";

const KEY_ALIASES: Record<string, string> = {
  ctrl: "ControlLeft",
  control: "ControlLeft",
  alt: "AltLeft",
  altgr: "AltRight",
  shift: "ShiftLeft",
  win: "MetaLeft",
  windows: "MetaLeft",
  meta: "MetaLeft",
  super: "MetaLeft",
  cmd: "MetaLeft",
  enter: "Enter",
  return: "Enter",
  esc: "Escape",
  escape: "Escape",
  tab: "Tab",
  space: "Space",
  backspace: "Backspace",
  delete: "Delete",
  del: "Delete",
  insert: "Insert",
  ins: "Insert",
  home: "Home",
  end: "End",
  pageup: "PageUp",
  pgup: "PageUp",
  pagedown: "PageDown",
  pgdn: "PageDown",
  up: "ArrowUp",
  down: "ArrowDown",
  left: "ArrowLeft",
  right: "ArrowRight",
  printscreen: "PrintScreen",
  menu: "ContextMenu",
  capslock: "CapsLock",
};

/** "ctrl+shift+esc" → the scancodes to hold, in order; `null` if a key is unknown. */
function keyCodes(keys: string): number[] | null {
  const names = keys
    .toLowerCase()
    .split("+")
    .map((k) => k.trim())
    .filter(Boolean);
  if (names.length === 0) return null;
  const codes: number[] = [];
  for (const name of names) {
    let code = KEY_ALIASES[name];
    if (!code && /^[a-z]$/.test(name)) code = `Key${name.toUpperCase()}`;
    if (!code && /^[0-9]$/.test(name)) code = `Digit${name}`;
    if (!code && /^f([1-9]|1[0-2])$/.test(name)) code = name.toUpperCase();
    const scancode = code ? SCANCODES[code] : undefined;
    if (scancode === undefined) return null;
    codes.push(scancode);
  }
  return codes;
}

const BUTTONS = { left: 0, middle: 1, right: 2 } as const;
const sleep = (ms: number) => new Promise((resolve) => setTimeout(resolve, ms));

/** Model coordinates (screenshot pixels) → desktop pixels. */
function toDesktop(shot: RdpScreenshot, x: number, y: number) {
  const maxX = Math.round(shot.width * shot.scale) - 1;
  const maxY = Math.round(shot.height * shot.scale) - 1;
  return {
    x: Math.max(0, Math.min(maxX, Math.round(x * shot.scale))),
    y: Math.max(0, Math.min(maxY, Math.round(y * shot.scale))),
  };
}

function typeOps(text: string): RdpInput[] {
  const ops: RdpInput[] = [];
  for (const char of text.replace(/\r\n?/g, "\n")) {
    const special = char === "\n" ? SCANCODES.Enter : char === "\t" ? SCANCODES.Tab : undefined;
    if (special !== undefined) ops.push({ type: "key", scancode: special, pressed: true }, { type: "key", scancode: special, pressed: false });
    else ops.push({ type: "unicode", text: char, pressed: true }, { type: "unicode", text: char, pressed: false });
  }
  return ops;
}

/**
 * Does the action on the desktop in `tabId`, waits for the screen to settle and returns a new
 * screenshot (also for "screenshot" itself). Throws when the desktop is not connected.
 */
export async function performAction(tabId: string, action: DesktopAction): Promise<RdpScreenshot> {
  const rdp = getRdp(tabId);
  if (!rdp) throw new Error("the remote desktop is not connected");
  const shot = lastShots.get(tabId) ?? takeScreenshot(tabId);
  if (!shot) throw new Error("the remote desktop is not showing yet");

  switch (action.action) {
    case "click": {
      const p = toDesktop(shot, action.x, action.y);
      const button = BUTTONS[action.button ?? "left"];
      const ops: RdpInput[] = [{ type: "mouseMove", ...p }];
      for (let i = 0; i < (action.clicks ?? 1); i++) {
        ops.push({ type: "mouseButton", button, pressed: true }, { type: "mouseButton", button, pressed: false });
      }
      rdp.input(ops);
      break;
    }
    case "move":
      rdp.input([{ type: "mouseMove", ...toDesktop(shot, action.x, action.y) }]);
      break;
    case "drag": {
      const from = toDesktop(shot, action.x, action.y);
      const to = toDesktop(shot, action.toX, action.toY);
      rdp.input([{ type: "mouseMove", ...from }, { type: "mouseButton", button: 0, pressed: true }]);
      // Some programs only notice a drag through the moves in between.
      for (let step = 1; step <= 8; step++) {
        await sleep(30);
        rdp.input([{ type: "mouseMove", x: Math.round(from.x + ((to.x - from.x) * step) / 8), y: Math.round(from.y + ((to.y - from.y) * step) / 8) }]);
      }
      await sleep(30);
      rdp.input([{ type: "mouseButton", button: 0, pressed: false }]);
      break;
    }
    case "scroll":
      rdp.input([
        { type: "mouseMove", ...toDesktop(shot, action.x, action.y) },
        { type: "wheel", vertical: true, units: Math.max(-32768, Math.min(32767, -action.amount * 120)) },
      ]);
      break;
    case "type":
      rdp.input(typeOps(action.text));
      break;
    case "key": {
      const codes = keyCodes(action.keys) ?? [];
      rdp.input([
        ...codes.map((scancode) => ({ type: "key" as const, scancode, pressed: true })),
        ...codes
          .slice()
          .reverse()
          .map((scancode) => ({ type: "key" as const, scancode, pressed: false })),
      ]);
      break;
    }
    case "wait":
      await sleep(action.seconds * 1000);
      break;
    case "screenshot":
      break;
  }

  if (action.action !== "screenshot" && action.action !== "wait") await sleep(SETTLE_MS);
  const next = takeScreenshot(tabId);
  if (!next) throw new Error("the remote desktop is not connected");
  return next;
}
