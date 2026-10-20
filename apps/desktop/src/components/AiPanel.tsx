import { useEffect, useLayoutEffect, useRef, useState, type KeyboardEvent, type ReactNode } from "react";
import { useTranslation } from "react-i18next";
import {
  Bot,
  ChevronRight,
  CircleAlert,
  CircleCheck,
  CircleX,
  CornerDownLeft,
  LoaderCircle,
  Monitor,
  MousePointerClick,
  Paperclip,
  Play,
  RotateCcw,
  SlidersHorizontal,
  Sparkles,
  Square,
  SquareTerminal,
  TextCursorInput,
  TriangleAlert,
  X,
} from "lucide-react";
import { cn } from "@hostealo/ui/cn";

import { isDangerousCommand } from "../lib/danger";
import { describeAction, isRiskyAction, parseAction, takeScreenshot } from "../lib/desktopAgent";
import { isShellLanguage, parseInline, parseMarkdown } from "../lib/markdown";
import { getTerminal } from "../lib/terminals";
import type { AiImage, Host } from "../lib/ipc";
import { agentReach, useAi, type AiMessage } from "../store/ai";
import { useHosts } from "../store/hosts";
import { LOCAL_MONITOR, useMonitors } from "../store/monitor";
import { isTerminalTab, useTabs, type Tab } from "../store/tabs";
import { AiSettingsDialog } from "./AiSettingsDialog";
import { ConfirmDialog } from "./ConfirmDialog";
import { Button } from "./ui/Button";
import { CopyButton } from "./ui/CopyButton";

/** Lines of terminal output sent with each question. */
const CONTEXT_LINES = 150;
/** And at most this many characters of them (the most recent ones). */
const CONTEXT_CHARS = 12_000;

/** Describes the session in the active tab, for the assistant. */
function sessionContext(tab: Tab | undefined, host: Host | undefined, os: string | null | undefined, hostname: string | null | undefined) {
  const lines: string[] = [];
  if (!tab || tab.kind === "local") lines.push("Session: a local shell on the user's own computer");
  else if (host) {
    const kind = { ssh: "SSH", sftp: "SFTP", ftp: host.protocol === "ftps" ? "FTPS" : "FTP", rdp: "remote desktop (RDP)" }[tab.kind as "ssh"];
    lines.push(`Session: ${kind} to "${host.name}" (${host.username ? `${host.username}@` : ""}${host.hostname}:${host.port})`);
  }
  if (hostname) lines.push(`Hostname: ${hostname}`);
  if (os) lines.push(`System: ${os}`);
  return lines.join("\n");
}

/** Keeps the end of `text` when it is too long. */
const tail = (text: string, max: number) => (text.length > max ? `…${text.slice(text.length - max)}` : text);

export function AiPanel() {
  const { t } = useTranslation();
  const {
    settings,
    loadSettings,
    setOpen,
    includeOutput,
    setIncludeOutput,
    agentMode,
    setAgentMode,
    desktopControl,
    setDesktopControl,
    autoApprove,
    setAutoApprove,
    send,
    stop,
    clear,
  } = useAi();
  const tabs = useTabs((s) => s.tabs);
  const activeId = useTabs((s) => s.activeId);
  const hosts = useHosts((s) => s.hosts);
  const monitors = useMonitors((s) => s.entries);
  const [settingsOpen, setSettingsOpen] = useState(false);
  const [draft, setDraft] = useState("");
  const inputRef = useRef<HTMLTextAreaElement>(null);
  const scrollRef = useRef<HTMLDivElement>(null);

  const tab = tabs.find((candidate) => candidate.id === activeId);
  const key = tab?.id ?? "general";
  const conversation = useAi((s) => s.conversations[key]);
  const messages = conversation?.messages ?? [];
  // A command the agent is running also keeps the conversation busy.
  const busy = (conversation?.requestId ?? null) !== null || messages.some((m) => m.actionState === "running");
  const host = hosts.find((candidate) => candidate.id === tab?.hostId);
  const terminal = tab && isTerminalTab(tab) ? tab.id : null;
  const rdp = tab?.kind === "rdp" ? tab.id : null;
  const sessionName = !tab || tab.kind === "local" ? t("tabs.local") : (host?.name ?? "");
  // What the agent can do here: a terminal's commands, or on a remote desktop, SSH and/or the mouse.
  const reach = agentReach(key, desktopControl);
  const canAct = reach.run !== null || reach.desktop;
  const commandsHostName = hosts.find((h) => h.id === reach.commandsHost)?.name;
  const attachable = terminal !== null || rdp !== null;

  useEffect(() => {
    if (!settings) void loadSettings().catch(() => {});
  }, [settings, loadSettings]);

  useEffect(() => {
    inputRef.current?.focus();
  }, [key]);

  // Follow the reply as it streams, unless the user scrolled up to read.
  const stickRef = useRef(true);
  useLayoutEffect(() => {
    const scroller = scrollRef.current;
    if (scroller && stickRef.current) scroller.scrollTop = scroller.scrollHeight;
  }, [messages]);

  const provider = settings?.settings.provider;
  const configured =
    !!settings?.saved &&
    (provider === "claudeCode" || settings.settings.model.trim() !== "") &&
    (settings.hasKey || !(provider === "anthropic" || provider === "openai"));

  const submit = (question = draft) => {
    const text = question.trim();
    if (!text || busy || !configured) return;
    const monitor = monitors[tab?.kind === "ssh" ? tab.id : LOCAL_MONITOR]?.snapshot;
    // Only sessions this computer can see into have a system and hostname to report.
    const local = !tab || tab.kind === "local";
    const parts = [sessionContext(tab, host, local || tab?.kind === "ssh" ? monitor?.os : null, local || tab?.kind === "ssh" ? monitor?.hostname : null)];
    let label = t("ai.contextSession", { name: sessionName });
    const output = includeOutput ? getTerminal(terminal)?.recentOutput(CONTEXT_LINES).trim() : undefined;
    if (output) {
      parts.push(`Recent terminal output (last ${CONTEXT_LINES} lines at most, oldest first):\n${tail(output, CONTEXT_CHARS)}`);
      label = t("ai.contextOutput", { name: sessionName });
    }
    // On a remote desktop, the screen is the context.
    const shot = rdp && includeOutput ? takeScreenshot(rdp) : null;
    if (shot) {
      parts.push(`A screenshot of the remote desktop is attached (${shot.width}×${shot.height} pixels).`);
      label = t("ai.contextScreen", { name: sessionName });
    }
    stickRef.current = true;
    setDraft("");
    void send(key, text, { text: parts.join("\n\n"), label }, shot ? [shot.image] : undefined);
  };

  const onKeyDown = (e: KeyboardEvent<HTMLTextAreaElement>) => {
    if (e.key === "Enter" && !e.shiftKey && !e.nativeEvent.isComposing) {
      e.preventDefault();
      submit();
    }
  };

  const suggestions = terminal
    ? [t("ai.suggestions.explain"), t("ai.suggestions.failed"), t("ai.suggestions.disk")]
    : rdp
      ? [t("ai.rdp.suggestions.screen"), t("ai.rdp.suggestions.error"), t("ai.rdp.suggestions.services")]
      : [t("ai.suggestions.howto"), t("ai.suggestions.firewall")];
  const intro = rdp
    ? agentMode
      ? t(canAct ? "ai.rdp.agentIntro" : "ai.rdp.noReach")
      : t("ai.rdp.intro", { screen: t("ai.includeScreen", { name: sessionName }) })
    : t(agentMode && terminal ? "ai.agent.intro" : "ai.intro");

  return (
    <aside className="v2-shell-main my-2 mr-2 flex w-[400px] shrink-0 flex-col overflow-hidden compact:fixed compact:inset-0 compact:z-30 compact:m-0 compact:w-auto compact:rounded-none compact:border-0 compact:pt-[env(safe-area-inset-top)] compact:pb-[env(safe-area-inset-bottom)]" aria-label={t("ai.title")}>
      <header className="flex h-11 shrink-0 items-center gap-2 border-b border-border pl-4 pr-2">
        <Sparkles className="size-4 text-accent" />
        <span className="text-sm font-medium text-fg">{t("ai.title")}</span>
        {settings && configured && (
          <span className="truncate rounded-badge bg-muted px-1.5 py-0.5 font-mono text-[11px] text-fg-muted" title={settings.settings.model}>
            {provider === "claudeCode" ? `Claude Code${settings.settings.model ? ` · ${settings.settings.model}` : ""}` : settings.settings.model}
          </span>
        )}
        <div className="flex-1" />
        {messages.length > 0 && (
          <Button variant="ghost" size="icon" onClick={() => clear(key)} title={t("ai.clear")} aria-label={t("ai.clear")}>
            <RotateCcw className="size-3.5" />
          </Button>
        )}
        <Button variant="ghost" size="icon" onClick={() => setSettingsOpen(true)} title={t("ai.settings")} aria-label={t("ai.settings")}>
          <SlidersHorizontal className="size-3.5" />
        </Button>
        <Button variant="ghost" size="icon" onClick={() => setOpen(false)} title={t("common.close")} aria-label={t("common.close")}>
          <X className="size-3.5" />
        </Button>
      </header>

      <div
        ref={scrollRef}
        onScroll={(e) => {
          const el = e.currentTarget;
          stickRef.current = el.scrollHeight - el.scrollTop - el.clientHeight < 40;
        }}
        className="min-h-0 flex-1 overflow-y-auto px-4 py-4"
      >
        {!configured ? (
          <div className="flex h-full flex-col items-center justify-center gap-3 px-4 text-center">
            <div className="grid size-10 place-items-center rounded-card bg-accent/10 text-accent">
              <Sparkles className="size-5" />
            </div>
            <p className="text-sm font-medium text-fg">{t("ai.connectTitle")}</p>
            <p className="text-xs leading-relaxed text-fg-muted">{t("ai.connectBody")}</p>
            <Button variant="primary" size="sm" onClick={() => setSettingsOpen(true)}>
              {t("ai.connect")}
            </Button>
          </div>
        ) : messages.length === 0 ? (
          <div className="flex h-full flex-col justify-end gap-2">
            <p className="mb-1 text-xs leading-relaxed text-fg-muted">{intro}</p>
            {suggestions.map((suggestion) => (
              <button
                key={suggestion}
                type="button"
                onClick={() => submit(suggestion)}
                className="cursor-pointer rounded-btn border border-border bg-card px-3 py-2 text-left text-[13px] text-fg-secondary shadow-sm transition-colors hover:bg-muted hover:text-fg"
              >
                {suggestion}
              </button>
            ))}
          </div>
        ) : (
          <div className="flex flex-col gap-5">
            {messages.map((message) =>
              message.role === "user" ? (
                !message.hidden && <UserMessage key={message.id} message={message} />
              ) : message.role === "action" ? (
                <ActionCard key={message.id} conversation={key} action={message} />
              ) : (
                <AssistantMessage key={message.id} message={message} terminal={terminal} onSettings={() => setSettingsOpen(true)} />
              ),
            )}
          </div>
        )}
      </div>

      <div className="shrink-0 border-t border-border p-3">
        <div className="rounded-btn border border-input-border bg-input shadow-sm focus-within:border-fg-muted focus-within:ring-4 focus-within:ring-ring">
          <textarea
            ref={inputRef}
            value={draft}
            onChange={(e) => setDraft(e.target.value)}
            onKeyDown={onKeyDown}
            rows={2}
            disabled={!configured}
            placeholder={t("ai.placeholder")}
            className="block max-h-40 min-h-[3.25rem] w-full resize-none bg-transparent px-3 pt-2 text-sm text-fg outline-none placeholder:text-fg-muted/70 [field-sizing:content] disabled:cursor-not-allowed"
          />
          <div className="flex items-center gap-2 px-2 pb-2">
            <label
              className={cn(
                "flex min-w-0 cursor-pointer items-center gap-1.5 whitespace-nowrap rounded-badge px-1.5 py-1 text-xs transition-colors",
                includeOutput && attachable ? "text-fg-secondary" : "text-fg-muted",
                !attachable && "cursor-default opacity-60",
              )}
              title={rdp ? t("ai.includeScreenHint") : terminal ? t("ai.includeOutputHint") : t("ai.noOutputHint")}
            >
              <input
                type="checkbox"
                className="accent-[var(--accent)]"
                checked={includeOutput && attachable}
                disabled={!attachable}
                onChange={(e) => setIncludeOutput(e.target.checked)}
              />
              {rdp ? <Monitor className="size-3 shrink-0" /> : <Paperclip className="size-3 shrink-0" />}
              <span className="truncate">{rdp ? t("ai.includeScreen", { name: sessionName }) : t("ai.includeOutput", { name: sessionName })}</span>
            </label>
            {(terminal || rdp) && (
              <button
                type="button"
                role="switch"
                aria-checked={agentMode}
                onClick={() => setAgentMode(!agentMode)}
                title={t("ai.agent.hint")}
                className={cn(
                  "flex shrink-0 cursor-pointer items-center gap-1.5 whitespace-nowrap rounded-badge border px-2 py-1 text-xs font-medium transition-colors",
                  agentMode ? "border-accent/50 bg-accent/10 text-accent" : "border-transparent text-fg-muted hover:text-fg",
                )}
              >
                <Bot className="size-3.5" />
                {t("ai.agent.label")}
              </button>
            )}
            <div className="flex-1" />
            {busy ? (
              <Button size="sm" onClick={() => stop(key)} title={t("ai.stop")}>
                <Square className="size-3 fill-current" />
                {t("ai.stop")}
              </Button>
            ) : (
              <Button size="sm" variant="primary" onClick={() => submit()} disabled={!draft.trim() || !configured}>
                {t("ai.send")}
                <CornerDownLeft className="size-3.5" />
              </Button>
            )}
          </div>
        </div>
        {agentMode && rdp && (
          <>
            <label className="mt-2 flex cursor-pointer items-center gap-1.5 px-1 text-xs text-fg-muted" title={t("ai.rdp.desktopControlHint")}>
              <input type="checkbox" className="accent-[var(--accent)]" checked={desktopControl} onChange={(e) => setDesktopControl(e.target.checked)} />
              <MousePointerClick className="size-3 shrink-0" />
              {t("ai.rdp.desktopControl")}
            </label>
            {commandsHostName && (
              <p className="mt-1.5 flex items-center gap-1.5 px-1 text-xs text-fg-muted">
                <SquareTerminal className="size-3 shrink-0" />
                {t("ai.rdp.commandsVia", { name: commandsHostName })}
              </p>
            )}
          </>
        )}
        {agentMode && canAct && (
          <label className="mt-2 flex cursor-pointer items-center gap-1.5 px-1 text-xs text-fg-muted">
            <input type="checkbox" className="accent-[var(--accent)]" checked={autoApprove} onChange={(e) => setAutoApprove(e.target.checked)} />
            {t("ai.agent.autoApprove")}
          </label>
        )}
      </div>

      {settingsOpen && <AiSettingsDialog onClose={() => setSettingsOpen(false)} />}
    </aside>
  );
}

const imageUrl = (image: AiImage) => `data:${image.mediaType};base64,${image.data}`;

function UserMessage({ message }: { message: AiMessage }) {
  return (
    <div className="flex flex-col items-end gap-1">
      <div className="max-w-[90%] whitespace-pre-wrap break-words rounded-card bg-accent-button px-3 py-2 text-sm text-white">{message.text}</div>
      {message.images?.map((image, i) => (
        <img key={i} src={imageUrl(image)} alt="" className="max-h-28 max-w-[60%] rounded-btn border border-border object-contain" />
      ))}
      {message.contextLabel && (
        <span className="flex items-center gap-1 text-[11px] text-fg-muted" title={message.context}>
          <Paperclip className="size-3" />
          {message.contextLabel}
        </span>
      )}
    </div>
  );
}

function AssistantMessage({ message, terminal, onSettings }: { message: AiMessage; terminal: string | null; onSettings: () => void }) {
  const { t } = useTranslation();
  const blocks = parseMarkdown(message.text);
  const streaming = message.state === "thinking" || message.state === "streaming";

  if (message.state === "refused") {
    return (
      <Notice icon={<TriangleAlert className="size-4 text-amber" />} title={t("ai.refusedTitle")}>
        {t("ai.refusedBody")}
      </Notice>
    );
  }

  return (
    <div className="flex flex-col gap-3 text-sm leading-relaxed text-fg">
      {blocks.map((block, i) => {
        switch (block.type) {
          case "code":
            // Commands and desktop actions for the agent show as their own card.
            if (block.lang === "run" || block.lang === "action") return null;
            return <CodeBlock key={i} lang={block.lang} code={block.code} complete={block.complete && !streaming} terminal={terminal} />;
          case "heading":
            return (
              <p key={i} className="font-semibold text-fg">
                <Inline text={block.text} />
              </p>
            );
          case "list": {
            const List = block.ordered ? "ol" : "ul";
            return (
              <List key={i} className={cn("flex flex-col gap-1 pl-5", block.ordered ? "list-decimal" : "list-disc")}>
                {block.items.map((item, j) => (
                  <li key={j}>
                    <Inline text={item} />
                  </li>
                ))}
              </List>
            );
          }
          default:
            return (
              <p key={i} className="whitespace-pre-wrap break-words">
                <Inline text={block.text} />
              </p>
            );
        }
      })}

      {message.state === "thinking" && (
        <p className="flex items-center gap-2 text-fg-muted">
          <LoaderCircle className="size-3.5 animate-spin" />
          {t("ai.thinking")}
        </p>
      )}
      {message.state === "streaming" && <span className="inline-block h-4 w-1.5 animate-pulse rounded-sm bg-accent" aria-hidden />}
      {message.state === "stopped" && <p className="text-xs text-fg-muted">{t("ai.stopped")}</p>}
      {message.state === "error" && (
        <Notice icon={<CircleAlert className="size-4 text-red" />} title={t("ai.errorTitle")}>
          <span className="break-words">{message.error}</span>
          <button type="button" onClick={onSettings} className="mt-1 block cursor-pointer text-accent hover:underline">
            {t("ai.checkSettings")}
          </button>
        </Notice>
      )}
      {message.state === "done" && message.model && <p className="font-mono text-[11px] text-fg-muted">{message.model}</p>}
    </div>
  );
}

function Notice({ icon, title, children }: { icon: ReactNode; title: string; children: ReactNode }) {
  return (
    <div className="flex gap-2.5 rounded-card border border-border bg-deep px-3 py-2.5 text-xs leading-relaxed text-fg-secondary">
      <span className="mt-0.5 shrink-0">{icon}</span>
      <div className="min-w-0">
        <p className="font-medium text-fg">{title}</p>
        {children}
      </div>
    </div>
  );
}

function Inline({ text }: { text: string }) {
  return (
    <>
      {parseInline(text).map((part, i) =>
        part.type === "code" ? (
          <code key={i} className="rounded bg-muted px-1 py-px font-mono text-[12px] text-fg">
            {part.text}
          </code>
        ) : part.type === "bold" ? (
          <strong key={i} className="font-semibold text-fg">
            {part.text}
          </strong>
        ) : (
          <span key={i}>{part.text}</span>
        ),
      )}
    </>
  );
}

/** A command or snippet. Shell commands can be typed into the terminal, never without a click. */
function CodeBlock({ lang, code, complete, terminal }: { lang: string; code: string; complete: boolean; terminal: string | null }) {
  const { t } = useTranslation();
  const [confirming, setConfirming] = useState(false);
  const target = getTerminal(terminal);
  const command = code.replace(/\n+$/, "");
  const actionable = complete && !!target && isShellLanguage(lang) && command.trim() !== "";
  const dangerous = isDangerousCommand(command);

  const run = () => {
    target?.run(command);
    target?.focus();
  };

  return (
    <div className={cn("overflow-hidden rounded-card border bg-deep", dangerous && complete ? "border-red/40" : "border-border")}>
      <div className="flex items-center gap-1 border-b border-border py-1 pl-3 pr-1">
        <span className="font-mono text-[11px] text-fg-muted">{lang || "shell"}</span>
        {dangerous && complete && (
          <span className="ml-1 flex items-center gap-1 text-[11px] text-red">
            <TriangleAlert className="size-3" />
            {t("ai.dangerous")}
          </span>
        )}
        <div className="flex-1" />
        {complete && <CopyButton text={command} label={t("ai.copy")} variant="ghost" />}
        {actionable && (
          <>
            <Button
              size="sm"
              variant="ghost"
              onClick={() => {
                target.insert(command);
                target.focus();
              }}
              title={t("ai.insertHint")}
            >
              <TextCursorInput className="size-3.5" />
              {t("ai.insert")}
            </Button>
            <Button size="sm" variant="ghost" className="text-accent" onClick={() => (dangerous ? setConfirming(true) : run())} title={t("ai.runHint")}>
              <Play className="size-3.5" />
              {t("ai.run")}
            </Button>
          </>
        )}
      </div>
      <pre className="overflow-x-auto px-3 py-2.5 font-mono text-[12.5px] leading-relaxed text-fg">
        <code>{code}</code>
      </pre>

      {confirming && (
        <ConfirmDialog title={t("ai.confirmRunTitle")} confirmLabel={t("ai.run")} icon={<TriangleAlert className="size-4" />} onConfirm={run} onClose={() => setConfirming(false)}>
          <p className="mb-2">{t("ai.confirmRunBody")}</p>
          <pre className="overflow-x-auto rounded-btn bg-deep px-3 py-2 font-mono text-xs text-fg">{command}</pre>
        </ConfirmDialog>
      )}
    </div>
  );
}

/** A command the agent wants to run: approve or decline it, then see how it went. */
function ActionCard({ conversation, action }: { conversation: string; action: AiMessage }) {
  if (action.actionKind === "desktop") return <DesktopActionCard conversation={conversation} action={action} />;
  return <CommandActionCard conversation={conversation} action={action} />;
}

/** A mouse or keyboard step on the remote desktop: approve or decline it, then see the screen. */
function DesktopActionCard({ conversation, action }: { conversation: string; action: AiMessage }) {
  const { t } = useTranslation();
  const { approve, decline } = useAi();
  const [open, setOpen] = useState(false);
  const parsed = parseAction(action.text);
  const risky = parsed !== null && isRiskyAction(parsed);
  const state = action.actionState ?? "pending";

  return (
    <div className={cn("overflow-hidden rounded-card border bg-deep", risky && state === "pending" ? "border-red/50" : "border-border")}>
      <div className="flex items-center gap-2 border-b border-border px-3 py-1.5 text-[11px] font-medium text-fg-muted">
        <MousePointerClick className="size-3.5 text-accent" />
        {action.error === "limit" ? t("ai.agent.limit") : t(`ai.desktop.state.${state}`)}
        {risky && state === "pending" && (
          <span className="ml-auto flex items-center gap-1 text-red">
            <TriangleAlert className="size-3" />
            {t("ai.dangerous")}
          </span>
        )}
      </div>
      <pre className="whitespace-pre-wrap px-3 py-2.5 [overflow-wrap:anywhere] font-mono text-[12.5px] leading-relaxed text-fg">
        <code>{parsed ? describeAction(parsed) : action.text}</code>
      </pre>
      {state === "pending" && (
        <div className="flex justify-end gap-2 border-t border-border px-3 py-2">
          <Button size="sm" onClick={() => decline(conversation, action.id)}>
            {t("ai.agent.decline")}
          </Button>
          <Button size="sm" variant="primary" className={risky ? "bg-red hover:bg-red hover:opacity-90" : undefined} onClick={() => void approve(conversation, action.id)}>
            <Play className="size-3.5" />
            {t("ai.desktop.approve")}
          </Button>
        </div>
      )}
      {state === "running" && (
        <p className="flex items-center gap-2 border-t border-border px-3 py-2 text-xs text-fg-muted">
          <LoaderCircle className="size-3.5 animate-spin" />
          {t("ai.desktop.state.running")}
        </p>
      )}
      {state === "failed" && (
        <p className="border-t border-border px-3 py-2 text-xs text-red">{action.error === "invalid" ? t("ai.desktop.invalid") : action.error}</p>
      )}
      {state === "done" && action.resultImage && (
        <div className="border-t border-border">
          <button type="button" onClick={() => setOpen(!open)} className="flex w-full cursor-pointer items-center gap-1.5 px-3 py-1.5 text-left text-xs text-fg-muted hover:text-fg">
            <ChevronRight className={cn("size-3.5 transition-transform", open && "rotate-90")} />
            {t("ai.desktop.screenAfter")}
          </button>
          {open && <img src={imageUrl(action.resultImage)} alt="" className="block w-full border-t border-border" />}
        </div>
      )}
    </div>
  );
}

/** A command the agent wants to run: approve or decline it, then see how it went. */
function CommandActionCard({ conversation, action }: { conversation: string; action: AiMessage }) {
  const { t } = useTranslation();
  const { approve, decline } = useAi();
  const [open, setOpen] = useState(false);
  const dangerous = isDangerousCommand(action.text);
  const output = action.output;
  const exit = output?.timedOut ? "timeout" : output?.exitCode;
  const ok = exit === 0;
  const text = output ? [output.stdout.trimEnd(), output.stderr.trimEnd()].filter(Boolean).join("\n") : "";

  return (
    <div className={cn("overflow-hidden rounded-card border bg-deep", dangerous && action.actionState === "pending" ? "border-red/50" : "border-border")}>
      <div className="flex items-center gap-2 border-b border-border px-3 py-1.5 text-[11px] font-medium text-fg-muted">
        <Bot className="size-3.5 text-accent" />
        {action.error === "limit" ? t("ai.agent.limit") : t(`ai.agent.state.${action.actionState}`)}
        {dangerous && action.actionState === "pending" && (
          <span className="ml-auto flex items-center gap-1 text-red">
            <TriangleAlert className="size-3" />
            {t("ai.dangerous")}
          </span>
        )}
        {action.actionState === "done" && (
          <span className={cn("ml-auto flex items-center gap-1", ok ? "text-green" : "text-red")}>
            {ok ? <CircleCheck className="size-3" /> : <CircleX className="size-3" />}
            {exit === "timeout" ? t("ai.agent.timeout") : t("ai.agent.exit", { code: exit ?? "?" })}
          </span>
        )}
      </div>
      {/* Wrapped, never cut: the whole command is visible before approving it. */}
      <pre className="whitespace-pre-wrap px-3 py-2.5 [overflow-wrap:anywhere] font-mono text-[12.5px] leading-relaxed text-fg">
        <code>{action.text}</code>
      </pre>
      {action.actionState === "pending" && (
        <div className="flex justify-end gap-2 border-t border-border px-3 py-2">
          <Button size="sm" onClick={() => decline(conversation, action.id)}>
            {t("ai.agent.decline")}
          </Button>
          <Button size="sm" variant="primary" className={dangerous ? "bg-red hover:bg-red hover:opacity-90" : undefined} onClick={() => void approve(conversation, action.id)}>
            <Play className="size-3.5" />
            {t("ai.agent.approve")}
          </Button>
        </div>
      )}
      {action.actionState === "running" && (
        <p className="flex items-center gap-2 border-t border-border px-3 py-2 text-xs text-fg-muted">
          <LoaderCircle className="size-3.5 animate-spin" />
          {t("ai.agent.running")}
        </p>
      )}
      {action.actionState === "failed" && <p className="border-t border-border px-3 py-2 text-xs text-red">{action.error}</p>}
      {action.actionState === "done" && (
        <div className="border-t border-border">
          <button type="button" onClick={() => setOpen(!open)} className="flex w-full cursor-pointer items-center gap-1.5 px-3 py-1.5 text-left text-xs text-fg-muted hover:text-fg">
            <ChevronRight className={cn("size-3.5 transition-transform", open && "rotate-90")} />
            {text ? t("ai.agent.output", { count: text.split("\n").length }) : t("ai.agent.noOutput")}
          </button>
          {open && text && (
            <pre className="max-h-64 overflow-auto border-t border-border px-3 py-2 font-mono text-[11.5px] leading-relaxed text-fg-secondary">{text}</pre>
          )}
        </div>
      )}
    </div>
  );
}
