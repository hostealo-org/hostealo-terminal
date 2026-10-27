import { useEffect, useMemo, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { Terminal } from "@xterm/xterm";
import { FitAddon } from "@xterm/addon-fit";
import { WebglAddon } from "@xterm/addon-webgl";
import { WebLinksAddon } from "@xterm/addon-web-links";
import { openUrl } from "@tauri-apps/plugin-opener";
import { hostealoDark, hostealoLight } from "@hostealo/ui/terminal-themes";
import { tintTerminalTheme } from "@hostealo/ui/host-colors";
import { cn } from "@hostealo/ui/cn";
import "@xterm/xterm/css/xterm.css";

import { OutputHighlighter } from "../lib/highlight";
import { createBackend, type SessionExit } from "../lib/sessions";
import { matchShortcut } from "../lib/shortcuts";
import { registerTerminal } from "../lib/terminals";
import { applyTouchModifiers } from "../lib/touchKeys";
import { isIOS, isMac } from "../lib/format";
import { useHostAccent } from "../hooks/useHostColor";
import { useResolvedTheme } from "../hooks/usePreferences";
import { useSettings } from "../store/settings";
import { useTabs, type Tab } from "../store/tabs";
import { TerminalTextSheet } from "./TerminalTextSheet";
import { Menu, type MenuAnchor } from "./ui/Menu";

const FONT_FAMILY = '"Geist Mono Variable", ui-monospace, "SF Mono", Menlo, Consolas, monospace';

const dim = (text: string) => `\x1b[2m${text.replace(/\r?\n/g, "\r\n")}\x1b[0m\r\n`;
const red = (text: string) => `\x1b[31m${text.replace(/\r?\n/g, "\r\n")}\x1b[0m\r\n`;

interface TerminalViewProps {
  tab: Tab;
  active: boolean;
}

/** The font sizes a pinch can reach. */
const MIN_FONT = 8;
const MAX_FONT = 28;
/** Time away from the app after which a dropped session reconnects on return. */
const AWAY_MS = 5_000;
/** How long after coming back a session that drops still counts as dropped while away. */
const RESUME_WINDOW_MS = 30_000;

export function TerminalView({ tab, active }: TerminalViewProps) {
  const { t } = useTranslation();
  const containerRef = useRef<HTMLDivElement>(null);
  const termRef = useRef<Terminal | null>(null);
  const fitRef = useRef<FitAddon | null>(null);
  const theme = useResolvedTheme();
  const accent = useHostAccent(tab.kind === "ssh" ? tab.hostId : null);
  const fontSize = useSettings((s) => s.fontSize);
  const screenReader = useSettings((s) => s.screenReader);
  const termTheme = useMemo(() => {
    const base = theme === "light" ? hostealoLight : hostealoDark;
    return accent ? tintTerminalTheme(base, accent.color, theme) : base;
  }, [theme, accent?.color]);

  // Read through refs so a language change does not restart the session.
  const tRef = useRef(t);
  tRef.current = t;
  const tabRef = useRef(tab);
  tabRef.current = tab;
  // Opens the terminal text window from the key handler, set up once with the terminal.
  const openTextSheet = useRef(() => {});

  useEffect(() => {
    const container = containerRef.current!;
    const settings = useSettings.getState();
    const term = new Terminal({
      fontFamily: FONT_FAMILY,
      fontSize: settings.fontSize,
      lineHeight: 1.2,
      cursorBlink: true,
      scrollback: 10_000,
      macOptionIsMeta: true,
      // Programs that use the mouse (Claude Code, vim, htop...) get the clicks; ⌥ (Shift
      // elsewhere, built into xterm) still selects text, as in iTerm and other terminals.
      macOptionClickForcesSelection: true,
      screenReaderMode: settings.screenReader,
      allowProposedApi: true,
      theme: document.documentElement.dataset.theme === "light" ? hostealoLight : hostealoDark,
    });
    const fit = new FitAddon();
    term.loadAddon(fit);
    term.loadAddon(
      new WebLinksAddon((event, uri) => {
        if (event.metaKey || event.ctrlKey) void openUrl(uri);
      }),
    );
    // App shortcuts are handled by the window listener in App; keep them away from the shell.
    // Copy and paste: ⌘C/⌘V on a Mac, Ctrl+Shift+C/V elsewhere (plain Ctrl+C is the shell's).
    term.attachCustomKeyEventHandler((e) => {
      if (e.type === "keydown" && isClipboardKey(e, "KeyC") && term.hasSelection()) {
        void navigator.clipboard.writeText(term.getSelection());
        return false;
      }
      if (e.type === "keydown" && e.shiftKey && isClipboardKey(e, "KeyS")) {
        openTextSheet.current();
        return false;
      }
      if (e.type === "keydown" && !isMac && isClipboardKey(e, "KeyV")) {
        void navigator.clipboard.readText().then((text) => text && term.paste(text));
        return false;
      }
      return !matchShortcut(e);
    });
    termRef.current = term;
    fitRef.current = fit;

    const backend = createBackend(tabRef.current);
    let disposed = false;
    let exited = false;

    const describeExit = ({ code, error }: SessionExit) => {
      const t = tRef.current;
      if (tabRef.current.kind === "ssh") {
        return error ? red(t("ssh.failed", { error })) : dim(t("ssh.closed"));
      }
      if (error) return red(t("terminal.spawnFailed", { error }));
      return dim(code === null ? t("terminal.exitedUnknown") : t("terminal.exited", { code }));
    };

    const start = () => {
      exited = false;
      // A fresh session starts with a fresh parser state (colours, alternate screen...).
      const highlighter = new OutputHighlighter();
      backend.start(term.cols, term.rows, {
        onData: (data) => term.write(highlighter.process(data, useSettings.getState().highlightOutput)),
        onInfo: (message) => term.write(dim(message)),
        onExit: (exit) => {
          if (disposed) return;
          exited = true;
          // Dropped while the app was in the background (iOS cuts connections there): pick it
          // up again by itself, keeping what the screen showed.
          if (tabRef.current.kind === "ssh" && isIOS && resumedRecently()) return reconnect();
          term.write(`\r\n${describeExit(exit)}`);
        },
      });
    };

    // Coming back to the app after a while: a session that died meanwhile reconnects, once.
    let hiddenAt = 0;
    let resumedAt = 0;
    let reconnected = false;
    const resumedRecently = () => !reconnected && resumedAt > 0 && Date.now() - resumedAt < RESUME_WINDOW_MS;
    const reconnect = () => {
      reconnected = true;
      term.write(`\r\n${dim(tRef.current("ssh.reconnecting"))}\r\n`);
      start();
    };
    const onVisibility = () => {
      if (document.hidden) {
        hiddenAt = Date.now();
        return;
      }
      if (!hiddenAt || Date.now() - hiddenAt < AWAY_MS) return;
      resumedAt = Date.now();
      reconnected = false;
      if (exited && tabRef.current.kind === "ssh" && isIOS) reconnect();
    };
    document.addEventListener("visibilitychange", onVisibility);

    term.onData((data) => {
      if (exited) {
        if (data === "\r") {
          term.reset();
          start();
        }
        return;
      }
      backend.write(applyTouchModifiers(data));
    });
    term.onResize(({ cols, rows }) => backend.resize(cols, rows));

    // The AI assistant reads the output and types commands, only when the user clicks.
    const unregister = registerTerminal(tabRef.current.id, {
      recentOutput: (count) => {
        const buffer = term.buffer.active;
        const lines: string[] = [];
        for (let i = Math.max(0, buffer.length - count); i < buffer.length; i++) {
          const line = buffer.getLine(i);
          if (!line) continue;
          const text = line.translateToString(false);
          // Rows wrapped by the terminal width are one line of output.
          if (line.isWrapped && lines.length) lines[lines.length - 1] += text;
          else lines.push(text);
        }
        return lines
          .map((line) => line.trimEnd())
          .join("\n")
          .replace(/\s+$/, "");
      },
      insert: (text) => {
        if (!exited) term.paste(text);
      },
      run: (text) => {
        if (!exited) backend.write(`${text.replace(/\r?\n/g, "\r")}\r`);
      },
      focus: () => term.focus(),
      send: (data) => {
        if (!exited) backend.write(data);
      },
      exec: (command, timeoutSecs) => backend.exec(command, timeoutSecs),
    });
    term.onTitleChange((title) => useTabs.getState().setTitle(tabRef.current.id, title));

    let frame = 0;
    const observer = new ResizeObserver(() => {
      cancelAnimationFrame(frame);
      frame = requestAnimationFrame(() => {
        if (container.clientWidth > 0 && container.clientHeight > 0) fit.fit();
      });
    });

    // xterm measures the cell size on open, so the font must be loaded first.
    void document.fonts.load(`${settings.fontSize}px "Geist Mono Variable"`).finally(() => {
      if (disposed) return;
      term.open(container);
      try {
        const webgl = new WebglAddon();
        webgl.onContextLoss(() => webgl.dispose());
        term.loadAddon(webgl);
      } catch {
        // No WebGL: xterm falls back to the DOM renderer.
      }
      fit.fit();
      observer.observe(container);
      start();
      if (useTabs.getState().activeId === tabRef.current.id) term.focus();
    });

    return () => {
      disposed = true;
      document.removeEventListener("visibilitychange", onVisibility);
      unregister();
      cancelAnimationFrame(frame);
      observer.disconnect();
      backend.dispose();
      term.dispose();
      termRef.current = null;
      fitRef.current = null;
    };
  }, [tab.id]);

  useEffect(() => {
    const term = termRef.current;
    if (!term) return;
    term.options.theme = termTheme;
  }, [termTheme]);

  useEffect(() => {
    const term = termRef.current;
    if (!term) return;
    term.options.fontSize = fontSize;
    fitRef.current?.fit();
  }, [fontSize]);

  useEffect(() => {
    if (termRef.current) termRef.current.options.screenReaderMode = screenReader;
  }, [screenReader]);

  // Two fingers pinching the terminal change its font size (touch screens), as in a browser.
  useEffect(() => {
    const container = containerRef.current;
    if (!container) return;
    let pinch: { distance: number; size: number } | null = null;
    const spread = (e: TouchEvent) => Math.hypot(e.touches[0].clientX - e.touches[1].clientX, e.touches[0].clientY - e.touches[1].clientY);
    const onStart = (e: TouchEvent) => {
      if (e.touches.length === 2) pinch = { distance: spread(e), size: useSettings.getState().fontSize };
    };
    const onMove = (e: TouchEvent) => {
      if (!pinch || e.touches.length !== 2) return;
      e.preventDefault();
      const size = Math.round(Math.min(MAX_FONT, Math.max(MIN_FONT, pinch.size * (spread(e) / pinch.distance))));
      if (size !== useSettings.getState().fontSize) useSettings.getState().update({ fontSize: size });
    };
    const onEnd = (e: TouchEvent) => {
      if (e.touches.length < 2) pinch = null;
    };
    container.addEventListener("touchstart", onStart, { passive: true });
    container.addEventListener("touchmove", onMove, { passive: false });
    container.addEventListener("touchend", onEnd);
    container.addEventListener("touchcancel", onEnd);
    return () => {
      container.removeEventListener("touchstart", onStart);
      container.removeEventListener("touchmove", onMove);
      container.removeEventListener("touchend", onEnd);
      container.removeEventListener("touchcancel", onEnd);
    };
  }, []);

  // A short message over the terminal ("Copied", which key selects...).
  const [notice, setNotice] = useState<string | null>(null);
  const noticeTimer = useRef(0);
  const showNotice = useRef((text: string) => {
    setNotice(text);
    window.clearTimeout(noticeTimer.current);
    noticeTimer.current = window.setTimeout(() => setNotice(null), 3500);
  }).current;
  useEffect(() => () => window.clearTimeout(noticeTimer.current), []);

  // The terminal's text in a window where the mouse selects it as in any document, whatever
  // the program running does with the mouse (⌘⇧S, or the right-click menu).
  const [textSheet, setTextSheet] = useState(false);
  openTextSheet.current = () => setTextSheet(true);

  // Mouse: a drag in a program that takes the mouse (Claude Code, vim...) selects nothing, so
  // say how to select anyway; and what is selected is copied on release (a setting).
  useEffect(() => {
    const container = containerRef.current;
    if (!container || isIOS) return;
    let start: { x: number; y: number } | null = null;
    let pressed = false;
    let copied = "";
    const onDown = (e: MouseEvent) => {
      const term = termRef.current;
      const forced = isMac ? e.altKey : e.shiftKey;
      pressed = e.button === 0;
      start = e.button === 0 && !forced && term && term.modes.mouseTrackingMode !== "none" ? { x: e.clientX, y: e.clientY } : null;
    };
    const onMove = (e: MouseEvent) => {
      if (!start || !(e.buttons & 1) || Math.hypot(e.clientX - start.x, e.clientY - start.y) < 8) return;
      start = null;
      showNotice(tRef.current(isMac ? "terminalMenu.selectHintMac" : "terminalMenu.selectHint"));
    };
    const onUp = () => {
      start = null;
      if (!pressed) return;
      pressed = false;
      const term = termRef.current;
      if (!term || !useSettings.getState().copyOnSelect || !term.hasSelection()) return;
      const text = term.getSelection();
      if (!text.trim() || text === copied) return;
      copied = text;
      void navigator.clipboard.writeText(text).then(() => showNotice(tRef.current("terminalMenu.copied")));
    };
    const selection = termRef.current?.onSelectionChange(() => {
      if (!termRef.current?.hasSelection()) copied = "";
    });
    container.addEventListener("mousedown", onDown, true);
    container.addEventListener("mousemove", onMove, true);
    window.addEventListener("mouseup", onUp);
    return () => {
      container.removeEventListener("mousedown", onDown, true);
      container.removeEventListener("mousemove", onMove, true);
      window.removeEventListener("mouseup", onUp);
      selection?.dispose();
    };
  }, [showNotice]);

  // Right click: copy, paste, select all, clear (on iPhone and iPad the key bar does this).
  const [menu, setMenu] = useState<MenuAnchor | null>(null);
  const menuEntries = () => {
    const term = termRef.current!;
    return [
      { label: t("terminalMenu.selectText"), hint: isMac ? "⇧⌘S" : "Ctrl+Shift+S", onSelect: () => setTextSheet(true) },
      { separator: true as const },
      ...(term.hasSelection()
        ? [{ label: t("terminalMenu.copy"), hint: isMac ? "⌘C" : "Ctrl+Shift+C", onSelect: () => void navigator.clipboard.writeText(term.getSelection()) }]
        : []),
      {
        label: t("terminalMenu.paste"),
        hint: isMac ? "⌘V" : "Ctrl+Shift+V",
        onSelect: () => void navigator.clipboard.readText().then((text) => text && term.paste(text)).finally(() => term.focus()),
      },
      { separator: true as const },
      { label: t("terminalMenu.selectAll"), onSelect: () => term.selectAll() },
      { label: t("terminalMenu.clear"), onSelect: () => term.clear() },
    ];
  };

  useEffect(() => {
    if (!active) return;
    const frame = requestAnimationFrame(() => {
      fitRef.current?.fit();
      termRef.current?.focus();
    });
    return () => cancelAnimationFrame(frame);
  }, [active]);

  return (
    // Inactive tabs stay mounted (and keep their session) but are hidden; `invisible` keeps
    // their size so xterm can still measure itself.
    <div
      className={cn("absolute inset-0 py-2 pl-3 pr-1", !active && "invisible")}
      style={{ backgroundColor: termTheme.background }}
    >
      {/* Coloured hosts get a stripe along the top, on top of the tinted background. */}
      {accent && <div className="pointer-events-none absolute inset-x-0 top-0 h-0.5" style={{ background: accent.hex }} />}
      <div
        ref={containerRef}
        className="size-full"
        onContextMenu={(e) => {
          if (isIOS || !termRef.current) return;
          e.preventDefault();
          setMenu({ x: e.clientX, y: e.clientY });
        }}
      />
      {notice && (
        <div role="status" className="pointer-events-none absolute bottom-4 left-1/2 max-w-[90%] -translate-x-1/2 rounded-full bg-black/80 px-4 py-2 text-center text-sm text-white shadow-lg">
          {notice}
        </div>
      )}
      {textSheet && (
        <TerminalTextSheet
          tabId={tab.id}
          onClose={() => {
            setTextSheet(false);
            termRef.current?.focus();
          }}
        />
      )}
      {menu && (
        <Menu
          anchor={menu}
          entries={menuEntries()}
          onClose={() => {
            setMenu(null);
            termRef.current?.focus();
          }}
        />
      )}
    </div>
  );
}

/** ⌘ + key on a Mac; Ctrl+Shift + key elsewhere, where Ctrl + key belongs to the shell. */
function isClipboardKey(e: KeyboardEvent, code: "KeyC" | "KeyV" | "KeyS") {
  if (e.code !== code || e.altKey) return false;
  return isMac ? e.metaKey && !e.ctrlKey : e.ctrlKey && e.shiftKey && !e.metaKey;
}
