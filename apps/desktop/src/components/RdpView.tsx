import { useCallback, useEffect, useMemo, useRef, useState, type KeyboardEvent, type MouseEvent, type PointerEvent, type WheelEvent } from "react";
import { useTranslation } from "react-i18next";
import { LoaderCircle, RefreshCw } from "lucide-react";
import { cn } from "@hostealo/ui/cn";

import { isIOS, isMac } from "../lib/format";
import { rdpClipboardSync, rdpClose, rdpConnect, rdpInput, rdpResize, type RdpClipboardNotice, type RdpInput } from "../lib/ipc";
import { closeRdpCommands } from "../lib/rdpCommands";
import { registerRdp, useRdpAudio } from "../lib/rdpSessions";
import { createTouchGestures, useRdpKeyboard } from "../lib/rdpTouch";
import { CTRL_ALT_DEL, SCANCODES } from "../lib/scancodes";
import { useHostAccent } from "../hooks/useHostColor";
import { usePrompts } from "../store/prompts";
import { useTabs, type Tab } from "../store/tabs";
import { RdpClipboardBar } from "./RdpClipboardBar";
import { RdpKeyboard } from "./RdpKeyboard";
import { Button } from "./ui/Button";

/** Wait this long after the last window resize before asking the server for a new size. */
const RESIZE_DEBOUNCE_MS = 600;
/** Wheel delta that makes one notch (a notch is 120 RDP units). */
const WHEEL_STEP = 50;
/** Below this difference in shape the desktop is stretched to fill the tab (no bars). */
const FILL_TOLERANCE = 0.04;

interface Size {
  width: number;
  height: number;
}

/** The desktop size to ask for: the tab's, in the steps servers accept (width a multiple of 4). */
const desktopSize = (el: HTMLElement): Size => ({
  width: Math.max(200, Math.floor(el.clientWidth / 4) * 4),
  height: Math.max(200, Math.floor(el.clientHeight / 2) * 2),
});
const sameSize = (a: Size | null, b: Size) => a !== null && a.width === b.width && a.height === b.height;

/** A remote desktop tab: the server's screen on a canvas, with mouse and keyboard forwarding. */
export function RdpView({ tab, active }: { tab: Tab; active: boolean }) {
  const { t } = useTranslation();
  const accent = useHostAccent(tab.hostId);
  const containerRef = useRef<HTMLDivElement>(null);
  const canvasRef = useRef<HTMLCanvasElement>(null);
  const keyFieldRef = useRef<HTMLTextAreaElement>(null);
  const sessionRef = useRef<number | null>(null);
  const queueRef = useRef<RdpInput[]>([]);
  const wheelRef = useRef(0);
  const [remoteSize, setRemoteSize] = useState<Size | null>(null);
  const [cursor, setCursor] = useState("default");
  const [error, setError] = useState<string | null>(null);
  const [attempt, setAttempt] = useState(0);
  const [session, setSession] = useState<number | null>(null);
  const [clipboardNotice, setClipboardNotice] = useState<RdpClipboardNotice | null>(null);
  const dismissNotice = useCallback(() => setClipboardNotice(null), []);
  /** The tab's size, to fill it when the desktop has (almost) the same shape. */
  const [viewSize, setViewSize] = useState<Size | null>(null);
  /** The size last asked of the server, so a resize is only asked once. */
  const requestedRef = useRef<Size | null>(null);

  // Input goes out in batches, at most once per frame.
  const frameRef = useRef(0);
  const send = (ops: RdpInput[]) => {
    queueRef.current.push(...ops);
    if (frameRef.current) return;
    frameRef.current = requestAnimationFrame(() => {
      frameRef.current = 0;
      const batch = queueRef.current.splice(0);
      if (sessionRef.current !== null && batch.length > 0) void rdpInput(sessionRef.current, batch).catch(() => {});
    });
  };

  // Connect (and reconnect when `attempt` changes).
  useEffect(() => {
    const tabs = useTabs.getState();
    const container = containerRef.current!;
    let id: number | null = null;
    let disposed = false;
    setError(null);
    setRemoteSize(null);
    setClipboardNotice(null);
    useRdpAudio.getState().set(tab.id, { state: "waiting" });
    tabs.setStatus(tab.id, "connecting");

    const draw = (data: Uint8Array) => {
      const canvas = canvasRef.current;
      const context = canvas?.getContext("2d");
      if (!canvas || !context) return;
      const view = new DataView(data.buffer, data.byteOffset, data.byteLength);
      const [a, b, c, d] = [1, 3, 5, 7].map((offset) => view.getUint16(offset, true));
      // Frames arrive in their own ArrayBuffer (never shared memory).
      const pixels = new Uint8ClampedArray(data.buffer as ArrayBuffer, data.byteOffset + 9, data.byteLength - 9);
      if (data[0] === 0) {
        // Desktop rectangle: x, y, width, height.
        if (c * d * 4 === pixels.length) context.putImageData(new ImageData(pixels, c, d), a, b);
      } else if (data[0] === 1 && a > 0 && b > 0 && a * b * 4 === pixels.length) {
        // Pointer bitmap: width, height, hotspot x, hotspot y.
        const pointer = document.createElement("canvas");
        pointer.width = a;
        pointer.height = b;
        pointer.getContext("2d")?.putImageData(new ImageData(pixels, a, b), 0, 0);
        setCursor(`url(${pointer.toDataURL()}) ${c} ${d}, default`);
      }
    };

    const initial = desktopSize(container);
    requestedRef.current = initial;
    rdpConnect(tab.hostId!, initial.width, initial.height, {
      onEvent: (event) => {
        if (disposed) return;
        switch (event.type) {
          case "status":
            tabs.setStatus(tab.id, event.stage);
            return;
          case "hostKey":
          case "certificate":
          case "prompt":
            usePrompts.getState().enqueue({ ...event, tabId: tab.id, hostId: tab.hostId! });
            return;
          case "closed":
            sessionRef.current = null;
            setSession(null);
            usePrompts.getState().dropForTab(tab.id);
            tabs.setStatus(tab.id, event.error ? "failed" : "closed");
            setError(event.error ?? t("rdp.disconnected"));
            return;
        }
      },
      onDisplay: (event) => {
        if (disposed) return;
        if (event.type === "audio") {
          useRdpAudio.getState().set(tab.id, event.playing ? { state: "playing" } : { state: "failed", error: event.error ?? "" });
        } else if (event.type === "clipboard") {
          setClipboardNotice(event.notice);
        } else if (event.type === "size") {
          const canvas = canvasRef.current;
          if (canvas) {
            canvas.width = event.width;
            canvas.height = event.height;
          }
          setRemoteSize({ width: event.width, height: event.height });
        } else {
          setCursor(event.type === "pointerHidden" ? "none" : "default");
        }
      },
      onFrame: (data) => {
        if (!disposed) draw(data);
      },
    })
      .then((opened) => {
        if (disposed) void rdpClose(opened);
        else {
          sessionRef.current = id = opened;
          setSession(opened);
        }
      })
      .catch((e) => {
        tabs.setStatus(tab.id, "failed");
        setError(String(e));
      });

    // Follow the tab size: servers with the display control channel resize the session live;
    // for the others the session reconnects at the new size (Windows resumes the same session).
    let timer: ReturnType<typeof setTimeout> | undefined;
    const observer = new ResizeObserver(() => {
      if (container.clientWidth > 0) setViewSize({ width: container.clientWidth, height: container.clientHeight });
      clearTimeout(timer);
      timer = setTimeout(() => {
        if (id === null || container.clientWidth === 0) return;
        // The on-screen keyboard takes room only for a while: the desktop just scales down
        // meanwhile (resizing would reconnect on some servers). Closing it brings the size back.
        if (useRdpKeyboard.getState().tabId === tab.id) return;
        const wanted = desktopSize(container);
        if (sameSize(requestedRef.current, wanted)) return;
        requestedRef.current = wanted;
        void rdpResize(id, wanted.width, wanted.height)
          .then((live) => {
            if (!live && !disposed) setAttempt((n) => n + 1);
          })
          .catch(() => {});
      }, RESIZE_DEBOUNCE_MS);
    });
    observer.observe(container);

    return () => {
      disposed = true;
      clearTimeout(timer);
      observer.disconnect();
      usePrompts.getState().dropForTab(tab.id);
      if (id !== null) void rdpClose(id);
      sessionRef.current = null;
      setSession(null);
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [tab.id, tab.hostId, attempt]);

  useEffect(() => {
    if (active) canvasRef.current?.focus();
  }, [active, remoteSize]);

  // The tab bar's Ctrl+Alt+Del and shared folder buttons act on this session.
  const sendRef = useRef(send);
  sendRef.current = send;
  useEffect(
    () =>
      registerRdp(tab.id, {
        sendCtrlAltDel: () => {
          sendRef.current([
            ...CTRL_ALT_DEL.map((scancode) => ({ type: "key" as const, scancode, pressed: true })),
            ...CTRL_ALT_DEL.slice().reverse().map((scancode) => ({ type: "key" as const, scancode, pressed: false })),
          ]);
          canvasRef.current?.focus();
        },
        reconnect: () => setAttempt((n) => n + 1),
        openKeyboard: () => {
          keyFieldRef.current?.focus();
          useRdpKeyboard.getState().setOpen(tab.id);
        },
        closeKeyboard: () => {
          keyFieldRef.current?.blur();
          useRdpKeyboard.getState().setOpen(null);
        },
        screenshot: (maxEdge) => {
          const canvas = canvasRef.current;
          if (!canvas || canvas.width === 0 || canvas.height === 0 || sessionRef.current === null) return null;
          const scale = Math.max(1, Math.max(canvas.width, canvas.height) / maxEdge);
          const shot = document.createElement("canvas");
          shot.width = Math.round(canvas.width / scale);
          shot.height = Math.round(canvas.height / scale);
          const context = shot.getContext("2d");
          if (!context) return null;
          context.imageSmoothingQuality = "high";
          context.drawImage(canvas, 0, 0, shot.width, shot.height);
          const data = shot.toDataURL("image/jpeg", 0.85).replace(/^data:image\/jpeg;base64,/, "");
          return { image: { mediaType: "image/jpeg", data }, width: shot.width, height: shot.height, scale: canvas.width / shot.width };
        },
        input: (ops) => sendRef.current(ops),
      }),
    [tab.id],
  );

  // The AI agent's SSH connection for this desktop goes away with the tab.
  useEffect(() => () => closeRdpCommands(tab.id), [tab.id]);

  // Stretch to fill when the shapes (almost) match; otherwise keep the shape.
  const fill =
    remoteSize !== null &&
    viewSize !== null &&
    Math.abs(remoteSize.width / remoteSize.height / (viewSize.width / viewSize.height) - 1) < FILL_TOLERANCE;

  /** A screen point in remote desktop pixels (the canvas may be scaled to fit). */
  const remotePoint = (clientX: number, clientY: number): { x: number; y: number } | null => {
    const canvas = canvasRef.current;
    if (!canvas || !remoteSize) return null;
    const rect = canvas.getBoundingClientRect();
    const x = Math.round(((clientX - rect.left) / rect.width) * remoteSize.width);
    const y = Math.round(((clientY - rect.top) / rect.height) * remoteSize.height);
    return { x: Math.max(0, Math.min(remoteSize.width - 1, x)), y: Math.max(0, Math.min(remoteSize.height - 1, y)) };
  };
  const position = (e: MouseEvent) => remotePoint(e.clientX, e.clientY);
  const remotePointRef = useRef(remotePoint);
  remotePointRef.current = remotePoint;

  // Fingers (iPhone, iPad): tap to click, hold for a right click, drag, two fingers to scroll.
  const touch = useMemo(
    () =>
      createTouchGestures(
        (ops) => sendRef.current(ops),
        (p) => remotePointRef.current(p.x, p.y),
      ),
    [],
  );
  const onTouch = (e: PointerEvent, phase: "down" | "move" | "up" | "cancel") => {
    if (e.pointerType !== "touch") return;
    // Also keeps the browser from making mouse events out of the touch.
    e.preventDefault();
    const p = { x: e.clientX, y: e.clientY };
    if (phase === "down") touch.down(e.pointerId, p);
    else if (phase === "move") touch.move(e.pointerId, p);
    else if (phase === "up") touch.up(e.pointerId, p);
    else touch.cancel();
  };

  // The device keyboard, opened from the tab bar (touch screens).
  const keyboardOpen = useRdpKeyboard((s) => s.tabId === tab.id);
  useEffect(() => {
    if (!active && useRdpKeyboard.getState().tabId === tab.id) useRdpKeyboard.getState().setOpen(null);
  }, [active, tab.id]);

  const onMouseMove = (e: MouseEvent) => {
    const p = position(e);
    if (p) send([{ type: "mouseMove", ...p }]);
  };

  const onMouseButton = (e: MouseEvent, pressed: boolean) => {
    e.preventDefault();
    canvasRef.current?.focus();
    const p = position(e);
    if (p) send([{ type: "mouseMove", ...p }, { type: "mouseButton", button: e.button, pressed }]);
  };

  const onWheel = (e: WheelEvent) => {
    // Trackpads send many small deltas: add them up into notches.
    wheelRef.current += e.deltaY;
    const notches = Math.trunc(wheelRef.current / WHEEL_STEP);
    if (notches === 0) return;
    wheelRef.current -= notches * WHEEL_STEP;
    send([{ type: "wheel", vertical: true, units: Math.max(-32768, Math.min(32767, -notches * 120)) }]);
  };

  const onKey = (e: KeyboardEvent, pressed: boolean) => {
    // Everything goes to the remote desktop, including Tab and the app's own shortcuts.
    e.preventDefault();
    e.stopPropagation();
    // On a Mac, ⌘ acts as Ctrl so ⌘C / ⌘V / ⌘A work as expected on Windows.
    const code = isMac && (e.code === "MetaLeft" || e.code === "MetaRight") ? "ControlLeft" : e.code;
    const scancode = SCANCODES[code];
    if (scancode !== undefined) send([{ type: "key", scancode, pressed }]);
    else if (e.key.length === 1) send([{ type: "unicode", text: e.key, pressed }]);
  };

  return (
    <div className="relative flex h-full flex-col bg-black">
      {accent && <div className="h-0.5 shrink-0" style={{ background: accent.hex }} />}
      <div ref={containerRef} className="relative grid min-h-0 flex-1 place-items-center overflow-hidden">
        <canvas
          ref={canvasRef}
          tabIndex={0}
          onMouseMove={onMouseMove}
          onMouseDown={(e) => onMouseButton(e, true)}
          onMouseUp={(e) => onMouseButton(e, false)}
          onPointerDown={(e) => onTouch(e, "down")}
          onPointerMove={(e) => onTouch(e, "move")}
          onPointerUp={(e) => onTouch(e, "up")}
          onPointerCancel={(e) => onTouch(e, "cancel")}
          onContextMenu={(e) => e.preventDefault()}
          onWheel={onWheel}
          onKeyDown={(e) => onKey(e, true)}
          onKeyUp={(e) => onKey(e, false)}
          onBlur={() => send([{ type: "releaseAll" }])}
          // Whatever was copied while away is offered to the remote again.
          onFocus={() => sessionRef.current !== null && void rdpClipboardSync(sessionRef.current).catch(() => {})}
          style={{ cursor, touchAction: "none", aspectRatio: remoteSize && !fill ? `${remoteSize.width} / ${remoteSize.height}` : undefined }}
          className={remoteSize ? cn("outline-none", fill ? "absolute inset-0 size-full" : "max-h-full max-w-full") : "hidden"}
        />

        {!remoteSize && (
          <div className="absolute inset-0 grid place-items-center bg-background">
            <div className="max-w-md px-6 text-center">
              {error ? (
                <>
                  <p className="text-sm font-medium text-fg">{t("rdp.connectFailed")}</p>
                  <p className="mt-1 break-words text-sm text-red">{error}</p>
                  <Button variant="primary" className="mt-4" onClick={() => setAttempt((n) => n + 1)}>
                    <RefreshCw className="size-4" />
                    {t("sftp.reconnect")}
                  </Button>
                </>
              ) : (
                <p className="flex items-center gap-2 text-sm text-fg-muted">
                  <LoaderCircle className="size-4 animate-spin" />
                  {t(`monitor.status.${tab.status === "connected" ? "connecting" : tab.status}`)}
                </p>
              )}
            </div>
          </div>
        )}

        {remoteSize && !error && <RdpClipboardBar session={session} notice={clipboardNotice} onDismiss={dismissNotice} />}

        {remoteSize && error && (
          <div className="absolute inset-x-0 bottom-4 mx-auto flex w-fit items-center gap-3 rounded-card border border-border bg-card px-4 py-2 text-sm shadow-md">
            <span className="text-fg-secondary">{error}</span>
            <Button size="sm" variant="primary" onClick={() => setAttempt((n) => n + 1)}>
              <RefreshCw className="size-3.5" />
              {t("sftp.reconnect")}
            </Button>
          </div>
        )}
      </div>

      {isIOS && remoteSize && (
        <RdpKeyboard
          field={keyFieldRef}
          open={keyboardOpen}
          send={send}
          onClose={() => {
            useRdpKeyboard.getState().setOpen(null);
            canvasRef.current?.focus();
          }}
        />
      )}
    </div>
  );
}
