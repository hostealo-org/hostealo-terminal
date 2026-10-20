import { create } from "zustand";

import { isDangerousCommand } from "../lib/danger";
import { actionBlockIn, describeAction, isPassiveAction, isRiskyAction, parseAction, performAction } from "../lib/desktopAgent";
import { aiCancel, aiChat, aiSettingsGet, type AiChatMessage, type AiImage, type AiSettingsView, type ExecOutput } from "../lib/ipc";
import { rdpCommandsExec } from "../lib/rdpCommands";
import { getRdp } from "../lib/rdpSessions";
import { getTerminal } from "../lib/terminals";
import { useHosts } from "./hosts";
import { useTabs } from "./tabs";

export type AiMessageState = "thinking" | "streaming" | "done" | "stopped" | "refused" | "error";
export type ActionState = "pending" | "running" | "done" | "declined" | "failed";

export interface AiMessage {
  id: string;
  /** `action`: a command the assistant wants to run, or a desktop action (agent mode). */
  role: "user" | "assistant" | "action";
  text: string;
  /** User messages: the terminal context sent along with the question, and what it was. */
  context?: string;
  contextLabel?: string;
  /** User messages: screenshots of the remote desktop sent with it. */
  images?: AiImage[];
  /** User messages the app wrote (command results): sent to the model, not shown as bubbles. */
  hidden?: boolean;
  /** Assistant messages. */
  state?: AiMessageState;
  model?: string;
  error?: string;
  /** Action messages: a shell command (`text` is the command) or a desktop action (`text` is its JSON). */
  actionKind?: "command" | "desktop";
  actionState?: ActionState;
  output?: ExecOutput;
  /** Desktop actions: the screen after it. */
  resultImage?: AiImage;
}

interface Conversation {
  messages: AiMessage[];
  /** The reply streaming right now (-1 while it starts). */
  requestId: number | null;
  /** The user pressed stop: a step still running finishes, but the agent does not go on. */
  halted?: boolean;
}

interface AiStore {
  open: boolean;
  setOpen: (open: boolean) => void;
  toggle: () => void;
  /** Send the terminal's recent output (or the remote desktop's screen) along with each question. */
  includeOutput: boolean;
  setIncludeOutput: (include: boolean) => void;
  /** The assistant runs commands itself, each one approved. */
  agentMode: boolean;
  setAgentMode: (on: boolean) => void;
  /** On remote desktops, the agent may also use the mouse and keyboard. */
  desktopControl: boolean;
  setDesktopControl: (on: boolean) => void;
  /** In agent mode, run commands that are not dangerous without asking. */
  autoApprove: boolean;
  setAutoApprove: (on: boolean) => void;
  settings: AiSettingsView | null;
  loadSettings: () => Promise<void>;
  setSettings: (settings: AiSettingsView) => void;
  /** One conversation per tab (or "general" when no tab is open). */
  conversations: Record<string, Conversation>;
  send: (key: string, question: string, context?: { text: string; label: string }, images?: AiImage[]) => Promise<void>;
  approve: (key: string, actionId: string) => Promise<void>;
  decline: (key: string, actionId: string) => void;
  stop: (key: string) => void;
  clear: (key: string) => void;
}

const EMPTY: Conversation = { messages: [], requestId: null };
/** Commands the agent may run in a row before it has to hand back to the user. */
export const MAX_AGENT_STEPS = 15;
/** Desktop tasks take many small steps (a click, a few keys...). */
export const MAX_DESKTOP_STEPS = 40;
/** How long one command may run. */
const COMMAND_TIMEOUT_SECS = 300;
/** Output sent back to the model per stream (its end). */
const OUTPUT_CHARS = 6000;
/** Screenshots resent with the conversation: the newest ones; older ones become a note. */
const KEEP_IMAGES = 2;

let nextId = 0;
const newId = () => `m${++nextId}`;

function readFlag(key: string, fallback: boolean): boolean {
  try {
    const value = localStorage.getItem(key);
    return value === null ? fallback : value === "true";
  } catch {
    return fallback;
  }
}

function writeFlag(key: string, value: boolean) {
  try {
    localStorage.setItem(key, String(value));
  } catch {
    // Not remembered, still applies now.
  }
}

const RUN_BLOCK = /```run[^\n]*\n([\s\S]*?)```/g;

/** The command the assistant asked to run: the last complete ```run block of its reply. */
export function commandIn(reply: string): string | null {
  const blocks = [...reply.matchAll(RUN_BLOCK)];
  const command = blocks.at(-1)?.[1].trim();
  return command ? command : null;
}

const tail = (text: string) => (text.length > OUTPUT_CHARS ? `…${text.slice(-OUTPUT_CHARS)}` : text);

/** A command's result, as the model reads it. */
export function describeOutput(command: string, output: ExecOutput): string {
  const parts = [`<command_output exit_code="${output.timedOut ? "timeout" : (output.exitCode ?? "unknown")}">`, `$ ${command}`];
  if (output.stdout.trim()) parts.push(`stdout:\n${tail(output.stdout.trimEnd())}`);
  if (output.stderr.trim()) parts.push(`stderr:\n${tail(output.stderr.trimEnd())}`);
  if (!output.stdout.trim() && !output.stderr.trim()) parts.push("(no output)");
  if (output.timedOut) parts.push(`(stopped after ${COMMAND_TIMEOUT_SECS} s)`);
  parts.push("</command_output>");
  return parts.join("\n");
}

/** What goes to the provider: the conversation so far, with each question's context. */
export function toChatMessages(messages: AiMessage[]): AiChatMessage[] {
  // Only the newest screenshots go out again: the older ones are history the model has seen.
  const withImages = messages.filter((m) => m.role === "user" && m.images?.length).map((m) => m.id);
  const keep = new Set(withImages.slice(-KEEP_IMAGES));
  const chat: AiChatMessage[] = [];
  for (const message of messages) {
    if (message.role === "user") {
      let content = message.context ? `<terminal_context>\n${message.context}\n</terminal_context>\n\n${message.text}` : message.text;
      if (message.images?.length && !keep.has(message.id)) content += "\n\n(An older screenshot was attached here.)";
      chat.push(keep.has(message.id) ? { role: "user", content, images: message.images } : { role: "user", content });
    } else if (message.role === "assistant" && (message.state === "done" || message.state === "stopped") && message.text.trim()) {
      // Failed and declined replies are left out (a declined partial is discarded).
      chat.push({ role: "assistant", content: message.text });
    }
  }
  return chat;
}

/** Commands run since the user last wrote. */
export function stepsSinceUser(messages: AiMessage[]): number {
  let steps = 0;
  for (let i = messages.length - 1; i >= 0; i--) {
    const message = messages[i];
    if (message.role === "user" && !message.hidden) break;
    if (message.role === "action") steps++;
  }
  return steps;
}

/** How the agent can act in a conversation: run commands, and use a remote desktop. */
export interface AgentReach {
  run: ((command: string, timeoutSecs: number) => Promise<ExecOutput>) | null;
  /** The saved SSH host commands go through, on a remote desktop tab. */
  commandsHost: string | null;
  desktop: boolean;
}

export function agentReach(key: string, desktopControl: boolean): AgentReach {
  const terminal = getTerminal(key);
  if (terminal) return { run: terminal.exec, commandsHost: null, desktop: false };
  const tab = useTabs.getState().tabs.find((t) => t.id === key);
  if (tab?.kind !== "rdp") return { run: null, commandsHost: null, desktop: false };
  const hosts = useHosts.getState().hosts;
  const linked = hosts.find((h) => h.id === tab.hostId)?.rdpCommandsHost;
  const commandsHost = linked && hosts.some((h) => h.id === linked && h.protocol === "ssh") ? linked : null;
  return {
    run: commandsHost ? (command, timeoutSecs) => rdpCommandsExec(key, commandsHost, command, timeoutSecs) : null,
    commandsHost,
    desktop: desktopControl && !!getRdp(key),
  };
}

export const useAi = create<AiStore>((set, get) => {
  const update = (key: string, fn: (conversation: Conversation) => Conversation) =>
    set((state) => ({ conversations: { ...state.conversations, [key]: fn(state.conversations[key] ?? EMPTY) } }));
  // A stopped reply ignores whatever the provider still sends.
  const updateMessage = (key: string, id: string, patch: (message: AiMessage) => Partial<AiMessage>) =>
    update(key, (c) => ({
      ...c,
      messages: c.messages.map((m) => (m.id === id && m.state !== "stopped" ? { ...m, ...patch(m) } : m)),
    }));
  const patchAction = (key: string, id: string, patch: Partial<AiMessage>) =>
    update(key, (c) => ({ ...c, messages: c.messages.map((m) => (m.id === id ? { ...m, ...patch } : m)) }));
  const append = (key: string, ...messages: AiMessage[]) => update(key, (c) => ({ ...c, messages: [...c.messages, ...messages] }));
  const find = (key: string, id: string) => get().conversations[key]?.messages.find((m) => m.id === id);
  const reach = (key: string) => agentReach(key, get().desktopControl);

  /** In agent mode, a finished reply may ask to run a command or act on the desktop. */
  const afterReply = (key: string, replyId: string) => {
    const reply = find(key, replyId);
    if (!get().agentMode || reply?.state !== "done") return;
    const can = reach(key);
    const runBlock = [...reply.text.matchAll(RUN_BLOCK)].at(-1);
    const actionBlock = actionBlockIn(reply.text);
    // Whichever comes last is what the reply ends with.
    const useAction = actionBlock !== null && can.desktop && (!runBlock || actionBlock.index > (runBlock.index ?? 0));
    const command = !useAction && can.run ? commandIn(reply.text) : null;
    if (!useAction && !command) return;

    const kind = useAction ? "desktop" : "command";
    const text = useAction ? actionBlock.raw : command!;
    const messages = get().conversations[key]?.messages ?? [];
    if (stepsSinceUser(messages) >= (kind === "desktop" ? MAX_DESKTOP_STEPS : MAX_AGENT_STEPS)) {
      // Hand back to the user: they say whether to go on.
      return append(key, { id: newId(), role: "action", actionKind: kind, text, actionState: "declined", error: "limit" });
    }

    if (kind === "desktop") {
      const action = parseAction(text);
      if (!action) {
        append(
          key,
          { id: newId(), role: "action", actionKind: kind, text, actionState: "failed", error: "invalid" },
          { id: newId(), role: "user", hidden: true, text: `<action_result>That action block is not a valid action, so nothing was done.</action_result>` },
        );
        return void request(key);
      }
      const card: AiMessage = { id: newId(), role: "action", actionKind: kind, text, actionState: "pending" };
      append(key, card);
      if (isPassiveAction(action) || (get().autoApprove && !isRiskyAction(action))) void get().approve(key, card.id);
      return;
    }

    const card: AiMessage = { id: newId(), role: "action", actionKind: kind, text, actionState: "pending" };
    append(key, card);
    if (get().autoApprove && !isDangerousCommand(text)) void get().approve(key, card.id);
  };

  /** Streams a reply to the conversation as it stands. */
  const request = async (key: string) => {
    const conversation = get().conversations[key] ?? EMPTY;
    const reply: AiMessage = { id: newId(), role: "assistant", text: "", state: "thinking" };
    const history = conversation.messages;
    update(key, () => ({ messages: [...history, reply], requestId: -1 }));
    const finish = () => {
      update(key, (c) => ({ ...c, requestId: null }));
      afterReply(key, reply.id);
    };
    // The model is told only about what it can really do here.
    const can = reach(key);
    const agentMode = get().agentMode;
    try {
      const requestId = await aiChat(toChatMessages(history), agentMode && can.run !== null, agentMode && can.desktop, (event) => {
        switch (event.type) {
          case "model":
            return updateMessage(key, reply.id, () => ({ model: event.model }));
          case "thinking":
            return;
          case "text":
            return updateMessage(key, reply.id, (m) => ({ text: m.text + event.text, state: "streaming" }));
          case "done":
            updateMessage(key, reply.id, () => ({ state: event.stopReason === "refusal" ? "refused" : "done" }));
            return finish();
          case "error":
            updateMessage(key, reply.id, () => ({ state: "error", error: event.message }));
            return finish();
        }
      });
      // The reply may already be over (an immediate error), or stopped before it started.
      if (find(key, reply.id)?.state === "stopped") void aiCancel(requestId);
      else update(key, (c) => (c.requestId === -1 ? { ...c, requestId } : c));
    } catch (e) {
      updateMessage(key, reply.id, () => ({ state: "error", error: String(e) }));
      update(key, (c) => ({ ...c, requestId: null }));
    }
  };

  const approveDesktop = async (key: string, action: AiMessage) => {
    const parsed = parseAction(action.text);
    if (!parsed) return;
    patchAction(key, action.id, { actionState: "running" });
    try {
      const shot = await performAction(key, parsed);
      if (find(key, action.id)?.actionState !== "running") return;
      patchAction(key, action.id, { actionState: "done", resultImage: shot.image });
      const done = parsed.action === "screenshot" ? "Here is the screen now" : `Done: ${describeAction(parsed)}. The screen now`;
      append(key, {
        id: newId(),
        role: "user",
        hidden: true,
        text: `<action_result>${done} (${shot.width}×${shot.height} pixels; coordinates refer to this screenshot).</action_result>`,
        images: [shot.image],
      });
    } catch (e) {
      patchAction(key, action.id, { actionState: "failed", error: String(e) });
      append(key, { id: newId(), role: "user", hidden: true, text: `<action_result>The action failed: ${String(e)}</action_result>` });
    }
    if (!get().conversations[key]?.halted) await request(key);
  };

  return {
    open: false,
    setOpen: (open) => set({ open }),
    toggle: () => set((state) => ({ open: !state.open })),
    includeOutput: readFlag("hostealo-terminal:ai-include-output", true),
    setIncludeOutput: (includeOutput) => {
      set({ includeOutput });
      writeFlag("hostealo-terminal:ai-include-output", includeOutput);
    },
    agentMode: readFlag("hostealo-terminal:ai-agent", false),
    setAgentMode: (agentMode) => {
      set({ agentMode });
      writeFlag("hostealo-terminal:ai-agent", agentMode);
    },
    desktopControl: readFlag("hostealo-terminal:ai-desktop", true),
    setDesktopControl: (desktopControl) => {
      set({ desktopControl });
      writeFlag("hostealo-terminal:ai-desktop", desktopControl);
    },
    autoApprove: readFlag("hostealo-terminal:ai-auto-approve", false),
    setAutoApprove: (autoApprove) => {
      set({ autoApprove });
      writeFlag("hostealo-terminal:ai-auto-approve", autoApprove);
    },
    settings: null,
    loadSettings: async () => set({ settings: await aiSettingsGet() }),
    setSettings: (settings) => set({ settings }),
    conversations: {},

    send: async (key, question, context, images) => {
      const conversation = get().conversations[key] ?? EMPTY;
      if (conversation.requestId !== null) return;
      // A new question leaves any command still waiting for approval unanswered.
      update(key, (c) => ({
        ...c,
        halted: false,
        messages: c.messages.map((m) => (m.actionState === "pending" ? { ...m, actionState: "declined" } : m)),
      }));
      append(key, { id: newId(), role: "user", text: question, context: context?.text, contextLabel: context?.label, images });
      await request(key);
    },

    approve: async (key, actionId) => {
      const action = find(key, actionId);
      if (!action || action.actionState !== "pending") return;
      // Approving a step means going on, even after an earlier stop.
      update(key, (c) => ({ ...c, halted: false }));
      if (action.actionKind === "desktop") return approveDesktop(key, action);
      const run = reach(key).run;
      if (!run) return;
      patchAction(key, actionId, { actionState: "running" });
      let output: ExecOutput;
      try {
        output = await run(action.text, COMMAND_TIMEOUT_SECS);
      } catch (e) {
        output = { stdout: "", stderr: String(e), exitCode: null, timedOut: false };
        patchAction(key, actionId, { actionState: "failed", error: String(e) });
      }
      if (find(key, actionId)?.actionState === "running") patchAction(key, actionId, { actionState: "done", output });
      append(key, { id: newId(), role: "user", hidden: true, text: describeOutput(action.text, output) });
      if (!get().conversations[key]?.halted) await request(key);
    },

    decline: (key, actionId) => {
      const action = find(key, actionId);
      if (!action || action.actionState !== "pending") return;
      patchAction(key, actionId, { actionState: "declined" });
      const what = action.actionKind === "desktop" ? `the desktop action ${action.text}` : `\`${action.text}\``;
      append(key, { id: newId(), role: "user", hidden: true, text: `I declined to run ${what}.` });
      void request(key);
    },

    stop: (key) => {
      const conversation = get().conversations[key];
      if (!conversation) return;
      if (conversation.requestId !== null && conversation.requestId > 0) void aiCancel(conversation.requestId);
      update(key, (c) => ({
        requestId: null,
        halted: true,
        messages: c.messages.map((m) =>
          m.state === "thinking" || m.state === "streaming"
            ? { ...m, state: "stopped" }
            : m.actionState === "pending"
              ? { ...m, actionState: "declined" }
              : m,
        ),
      }));
    },

    clear: (key) => {
      get().stop(key);
      update(key, () => EMPTY);
    },
  };
});
