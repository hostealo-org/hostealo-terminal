import { useEffect, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { Check, Copy, TextSelect, X } from "lucide-react";

import { isIOS, isMac } from "../lib/format";
import { getTerminal } from "../lib/terminals";
import { Button } from "./ui/Button";
import { Modal } from "./ui/Modal";

/** How much of the terminal's history the sheet shows. */
const LINES = 2000;

/**
 * The terminal's text as plain text, selectable as anywhere else, with Copy all: in a sheet on
 * iPhone and iPad (a finger cannot select inside the terminal), and in a window on desktop,
 * where programs that take the mouse (Claude Code, vim) keep it from selecting.
 */
export function TerminalTextSheet({ tabId, onClose }: { tabId: string; onClose: () => void }) {
  const { t } = useTranslation();
  const [text] = useState(() => getTerminal(tabId)?.recentOutput(LINES) ?? "");
  const [copied, setCopied] = useState(false);
  const copyAll = async () => {
    try {
      await navigator.clipboard.writeText(text);
      setCopied(true);
    } catch {
      setCopied(false);
    }
  };
  // The latest output is what is usually wanted: start at the bottom.
  const pre = useRef<HTMLPreElement>(null);
  useEffect(() => {
    if (pre.current) pre.current.scrollTop = pre.current.scrollHeight;
  }, []);
  const copyButton = (
    <Button variant={isIOS ? "secondary" : "primary"} size={isIOS ? "sm" : "md"} onClick={() => void copyAll()}>
      {copied ? <Check className={isIOS ? "size-3.5 text-green" : "size-3.5"} /> : <Copy className="size-3.5" />}
      {copied ? t("terminal.key.copied") : t("terminal.key.copyAll")}
    </Button>
  );

  if (!isIOS) {
    return (
      <Modal
        title={t("terminal.key.selectTitle")}
        icon={<TextSelect className="size-4" />}
        onClose={onClose}
        className="max-w-4xl"
        footer={
          <>
            <Button onClick={onClose}>{t("common.close")}</Button>
            {copyButton}
          </>
        }
      >
        <p className="text-xs text-fg-muted">{t(isMac ? "terminalMenu.sheetHintMac" : "terminalMenu.sheetHint")}</p>
        <pre
          ref={pre}
          tabIndex={0}
          data-autofocus
          // ⌘A / Ctrl+A selects the text, not the whole window.
          onKeyDown={(e) => {
            if (e.key.toLowerCase() !== "a" || !(isMac ? e.metaKey : e.ctrlKey)) return;
            e.preventDefault();
            window.getSelection()?.selectAllChildren(e.currentTarget);
          }}
          className="h-[60vh] select-text overflow-auto rounded-card border border-border bg-deep p-3 font-mono text-[13px] leading-relaxed whitespace-pre-wrap break-words text-fg outline-none"
        >
          {text}
        </pre>
      </Modal>
    );
  }

  return (
    <div className="fixed inset-0 z-50 flex flex-col bg-card pt-[env(safe-area-inset-top)] pb-[env(safe-area-inset-bottom)]" role="dialog" aria-modal="true" aria-label={t("terminal.key.selectTitle")}>
      <header className="flex h-13 shrink-0 items-center gap-2 border-b border-border px-3">
        <span className="min-w-0 flex-1 truncate text-sm font-semibold text-fg">{t("terminal.key.selectTitle")}</span>
        {copyButton}
        <Button variant="ghost" size="icon" onClick={onClose} aria-label={t("common.close")}>
          <X className="size-4" />
        </Button>
      </header>
      <p className="shrink-0 px-3 pt-2 text-xs text-fg-muted">{t("terminal.key.selectHint")}</p>
      {/* Selectable, unlike the rest of the app chrome. */}
      <pre ref={pre} data-own-touch className="min-h-0 flex-1 select-text overflow-auto px-3 py-2 font-mono text-[13px] leading-relaxed whitespace-pre-wrap break-words text-fg [-webkit-touch-callout:default] [-webkit-user-select:text]">
        {text}
      </pre>
    </div>
  );
}
