// Typed wrappers around the Rust commands in src-tauri/src.
import { Channel, invoke } from "@tauri-apps/api/core";

export interface DiskUsage {
  mount: string;
  used: number;
  total: number;
}

export interface ResourceSnapshot {
  hostname: string | null;
  os: string | null;
  cpuPercent: number;
  cpuCount: number;
  memUsed: number;
  memTotal: number;
  swapUsed: number;
  swapTotal: number;
  disks: DiskUsage[];
  netRxBps: number;
  netTxBps: number;
  load: [number, number, number] | null;
  uptimeSecs: number;
}

export function monitorLocal(): Promise<ResourceSnapshot> {
  return invoke("monitor_local");
}

/** Channel for raw byte streams (terminal output). */
function rawChannel(onData: (data: Uint8Array) => void): Channel<ArrayBuffer | number[]> {
  // Raw channel messages arrive as ArrayBuffer; keep the array fallback for older runtimes.
  const channel = new Channel<ArrayBuffer | number[]>();
  channel.onmessage = (chunk) => onData(chunk instanceof ArrayBuffer ? new Uint8Array(chunk) : Uint8Array.from(chunk));
  return channel;
}

export interface PtyHandlers {
  onData: (data: Uint8Array) => void;
  onExit: (code: number | null) => void;
}

export function ptySpawn(cols: number, rows: number, handlers: PtyHandlers): Promise<number> {
  const onExit = new Channel<number | null>();
  onExit.onmessage = handlers.onExit;

  return invoke("pty_spawn", { cols, rows, onData: rawChannel(handlers.onData), onExit });
}

export function ptyWrite(id: number, data: string): Promise<void> {
  return invoke("pty_write", { id, data });
}

export function ptyResize(id: number, cols: number, rows: number): Promise<void> {
  return invoke("pty_resize", { id, cols, rows });
}

export function ptyKill(id: number): Promise<void> {
  return invoke("pty_kill", { id });
}

export function ptyKillAll(): Promise<void> {
  return invoke("pty_kill_all");
}

// ── Hosts ────────────────────────────────────────────────────

export type AuthMethod = { type: "password" } | { type: "key"; keyPath: string } | { type: "agent" };
export type Protocol = "ssh" | "ftp" | "ftps" | "rdp";

/** `id` picks the logo (@hostealo/ui/os-icons), `name` is shown, e.g. "Ubuntu 24.04.1 LTS". */
export interface HostOs {
  id: string;
  name: string;
}

export interface Host {
  id: string;
  name: string;
  hostname: string;
  port: number;
  username: string;
  protocol: Protocol;
  auth: AuthMethod;
  /** Colour tag from @hostealo/ui/host-colors, or "none". `null` means the default colour. */
  color: string | null;
  /** Folder path such as "Clients/Acme"; `null` for the top level. */
  folder: string | null;
  /** RDP only: local folder shared with the remote desktop as a drive. */
  rdpSharedFolder: string | null;
  /** RDP only: the saved SSH host the AI agent runs commands through (same server, OpenSSH). */
  rdpCommandsHost?: string | null;
  /** Set by the backend: a secret is stored in the OS keychain. */
  hasPassword: boolean;
  hasPassphrase: boolean;
  /** Set by the backend: the operating system seen on the last SSH connection. */
  os?: HostOs | null;
  /** Set by the Hostealo sync: the service this host belongs to. */
  hostealo?: { serviceId: string; kind: "vps" | "dedicated" } | null;
}

export type SecretChange = { action: "keep" } | { action: "set"; value: string } | { action: "clear" };

export interface HostsData {
  hosts: Host[];
  /** Every folder path, including empty folders. */
  folders: string[];
  /** Quick commands. */
  snippets?: Snippet[];
}

/** A saved command, typed into a terminal with one click. */
export interface Snippet {
  /** Empty for a new one. */
  id: string;
  name: string;
  command: string;
  updatedAt?: number;
}

/** Creates or updates a quick command; returns them all. */
export const snippetsSave = (snippet: Snippet): Promise<Snippet[]> => invoke("snippets_save", { snippet });
export const snippetsDelete = (id: string): Promise<Snippet[]> => invoke("snippets_delete", { id });

export function hostsList(): Promise<HostsData> {
  return invoke("hosts_list");
}

export function foldersCreate(path: string): Promise<HostsData> {
  return invoke("folders_create", { path });
}

export function foldersRename(from: string, to: string): Promise<HostsData> {
  return invoke("folders_rename", { from, to });
}

/** Hosts and subfolders inside move up one level. */
export function foldersDelete(path: string): Promise<HostsData> {
  return invoke("folders_delete", { path });
}

export function hostsSave(host: Host, password: SecretChange, passphrase: SecretChange): Promise<Host> {
  return invoke("hosts_save", { host, password, passphrase });
}

export function hostsSetOs(id: string, os: HostOs): Promise<void> {
  return invoke("hosts_set_os", { id, os });
}

/** A saved secret, to show it in the host editor. On macOS, Touch ID or the Mac password first. */
export function hostsRevealSecret(id: string, kind: "password" | "passphrase", reason: string): Promise<string> {
  return invoke("hosts_reveal_secret", { id, kind, reason });
}

export function hostsDelete(id: string): Promise<void> {
  return invoke("hosts_delete", { id });
}

// ── SSH ──────────────────────────────────────────────────────

export type SshStage = "connecting" | "authenticating" | "connected";
export type PromptKind = "password" | "passphrase" | "keyboardInteractive";

export interface PromptField {
  prompt: string;
  echo: boolean;
}

export type SshEvent =
  | { type: "status"; stage: SshStage }
  | { type: "hostKey"; requestId: number; algorithm: string; fingerprint: string; changed: boolean }
  | { type: "certificate"; requestId: number; fingerprint: string; reason: string; changed: boolean }
  | {
      type: "prompt";
      requestId: number;
      kind: PromptKind;
      name: string;
      instructions: string;
      prompts: PromptField[];
      canRemember: boolean;
      retry: boolean;
    }
  | { type: "banner"; message: string }
  | { type: "os"; os: HostOs }
  | { type: "closed"; exitCode: number | null; error: string | null };

export type MonitorEvent = { type: "snapshot"; snapshot: ResourceSnapshot } | { type: "unsupported" };

export type PromptReply =
  | { type: "hostKey"; accept: boolean }
  | { type: "answers"; answers: string[]; remember: boolean }
  | { type: "cancel" };

export interface SshHandlers {
  onEvent: (event: SshEvent) => void;
  onData: (data: Uint8Array) => void;
  onMonitor: (event: MonitorEvent) => void;
}

export function sshConnect(hostId: string, cols: number, rows: number, handlers: SshHandlers): Promise<number> {
  const onEvent = new Channel<SshEvent>();
  onEvent.onmessage = handlers.onEvent;
  const onMonitor = new Channel<MonitorEvent>();
  onMonitor.onmessage = handlers.onMonitor;
  return invoke("ssh_connect", { hostId, cols, rows, onEvent, onData: rawChannel(handlers.onData), onMonitor });
}

export function sshWrite(id: number, data: string): Promise<void> {
  return invoke("ssh_write", { id, data });
}

export function sshResize(id: number, cols: number, rows: number): Promise<void> {
  return invoke("ssh_resize", { id, cols, rows });
}

export function sshClose(id: number): Promise<void> {
  return invoke("ssh_close", { id });
}

export function sshCloseAll(): Promise<void> {
  return invoke("ssh_close_all");
}

export function sshPromptReply(requestId: number, reply: PromptReply): Promise<void> {
  return invoke("ssh_prompt_reply", { requestId, reply });
}

// ── Known hosts ──────────────────────────────────────────────

export type KnownHostSource = "app" | "user";

export interface KnownHostEntry {
  source: KnownHostSource;
  line: number;
  raw: string;
  /** Empty for hashed entries. */
  hosts: string[];
  hashed: boolean;
  marker: string | null;
  keyType: string;
  fingerprint: string | null;
}

export function knownHostsList(query?: string): Promise<KnownHostEntry[]> {
  return invoke("known_hosts_list", { query: query || null });
}

export function knownHostsRemove(entries: Pick<KnownHostEntry, "source" | "line" | "raw">[]): Promise<number> {
  return invoke("known_hosts_remove", { entries });
}

/** Forgets every key of `target` (`host`, `host:port`), hashed entries included. */
export function knownHostsForget(target: string): Promise<number> {
  return invoke("known_hosts_forget", { target });
}

// ── SSH keys ─────────────────────────────────────────────────

export type KeyAlgorithm = "ed25519" | "ecdsa" | "rsa";

export interface SshKeyInfo {
  name: string;
  path: string;
  keyType: string;
  fingerprint: string | null;
  comment: string;
  encrypted: boolean;
  publicKey: string | null;
  modified: number | null;
}

export function sshKeysList(): Promise<SshKeyInfo[]> {
  return invoke("ssh_keys_list");
}

export function sshKeysGenerate(name: string, algorithm: KeyAlgorithm, passphrase: string, comment: string): Promise<SshKeyInfo> {
  return invoke("ssh_keys_generate", { name, algorithm, passphrase: passphrase || null, comment });
}

export function sshKeysDelete(path: string): Promise<void> {
  return invoke("ssh_keys_delete", { path });
}

export function sshKeysDefaultComment(): Promise<string> {
  return invoke("ssh_keys_default_comment");
}

// ── SFTP ─────────────────────────────────────────────────────

export interface SftpEntry {
  name: string;
  path: string;
  isDir: boolean;
  isSymlink: boolean;
  size: number;
  modified: number | null;
  permissions: number | null;
  owner: string | null;
}

export interface TransferProgress {
  transferred: number;
  total: number;
  filesDone: number;
  filesTotal: number;
  current: string;
}

export function sftpOpen(hostId: string, onEvent: (event: SshEvent) => void): Promise<number> {
  const channel = new Channel<SshEvent>();
  channel.onmessage = onEvent;
  return invoke("sftp_open", { hostId, onEvent: channel });
}

export function sftpClose(id: number): Promise<void> {
  return invoke("sftp_close", { id });
}

export function sftpRealpath(id: number, path: string): Promise<string> {
  return invoke("sftp_realpath", { id, path });
}

export function sftpList(id: number, path: string): Promise<SftpEntry[]> {
  return invoke("sftp_list", { id, path });
}

export function sftpMkdir(id: number, path: string): Promise<void> {
  return invoke("sftp_mkdir", { id, path });
}

export function sftpRename(id: number, from: string, to: string): Promise<void> {
  return invoke("sftp_rename", { id, from, to });
}

export function sftpDelete(id: number, path: string): Promise<void> {
  return invoke("sftp_delete", { id, path });
}

function progressChannel(onProgress: (progress: TransferProgress) => void): Channel<TransferProgress> {
  const channel = new Channel<TransferProgress>();
  channel.onmessage = onProgress;
  return channel;
}

export function sftpDownload(
  id: number,
  transferId: number,
  remote: string,
  localDir: string,
  onProgress: (progress: TransferProgress) => void,
): Promise<void> {
  return invoke("sftp_download", { id, transferId, remote, localDir, onProgress: progressChannel(onProgress) });
}

export function sftpUpload(
  id: number,
  transferId: number,
  locals: string[],
  remoteDir: string,
  onProgress: (progress: TransferProgress) => void,
): Promise<void> {
  return invoke("sftp_upload", { id, transferId, locals, remoteDir, onProgress: progressChannel(onProgress) });
}

export function sftpCancelTransfer(transferId: number): Promise<void> {
  return invoke("sftp_cancel_transfer", { transferId });
}

// ── FTP / FTPS ───────────────────────────────────────────────

export function ftpOpen(hostId: string, onEvent: (event: SshEvent) => void): Promise<number> {
  const channel = new Channel<SshEvent>();
  channel.onmessage = onEvent;
  return invoke("ftp_open", { hostId, onEvent: channel });
}

export const ftpClose = (id: number): Promise<void> => invoke("ftp_close", { id });
export const ftpRealpath = (id: number, path: string): Promise<string> => invoke("ftp_realpath", { id, path });
export const ftpList = (id: number, path: string): Promise<SftpEntry[]> => invoke("ftp_list", { id, path });
export const ftpMkdir = (id: number, path: string): Promise<void> => invoke("ftp_mkdir", { id, path });
export const ftpRename = (id: number, from: string, to: string): Promise<void> => invoke("ftp_rename", { id, from, to });
export const ftpDelete = (id: number, path: string, isDir: boolean): Promise<void> => invoke("ftp_delete", { id, path, isDir });
export const ftpCancelTransfer = (transferId: number): Promise<void> => invoke("ftp_cancel_transfer", { transferId });

export function ftpDownload(
  id: number,
  transferId: number,
  entry: SftpEntry,
  localDir: string,
  onProgress: (progress: TransferProgress) => void,
): Promise<void> {
  return invoke("ftp_download", {
    id,
    transferId,
    remote: entry.path,
    isDir: entry.isDir,
    localDir,
    onProgress: progressChannel(onProgress),
  });
}

export function ftpUpload(
  id: number,
  transferId: number,
  locals: string[],
  remoteDir: string,
  onProgress: (progress: TransferProgress) => void,
): Promise<void> {
  return invoke("ftp_upload", { id, transferId, locals, remoteDir, onProgress: progressChannel(onProgress) });
}

// ── RDP ──────────────────────────────────────────────────────

/** Files copied on the remote desktop, and the progress of saving them here. */
export type RdpClipboardNotice =
  | { kind: "remoteFiles"; names: string[]; count: number; bytes: number }
  | { kind: "progress"; done: number; total: number }
  | { kind: "saved"; paths: string[] }
  | { kind: "failed"; error: string }
  | { kind: "cancelled" };

export type RdpDisplayEvent =
  | { type: "size"; width: number; height: number }
  | { type: "pointerDefault" }
  | { type: "pointerHidden" }
  | { type: "clipboard"; notice: RdpClipboardNotice }
  /** Remote sound started playing here, or cannot be played (`error`). */
  | { type: "audio"; playing: boolean; error: string | null };

export type RdpInput =
  | { type: "mouseMove"; x: number; y: number }
  | { type: "mouseButton"; button: number; pressed: boolean }
  | { type: "wheel"; vertical: boolean; units: number }
  | { type: "key"; scancode: number; pressed: boolean }
  | { type: "unicode"; text: string; pressed: boolean }
  | { type: "releaseAll" };

export interface RdpHandlers {
  onEvent: (event: SshEvent) => void;
  onDisplay: (event: RdpDisplayEvent) => void;
  /** Raw frame messages, see src-tauri/src/rdp.rs. */
  onFrame: (data: Uint8Array) => void;
}

export function rdpConnect(hostId: string, width: number, height: number, handlers: RdpHandlers): Promise<number> {
  const onEvent = new Channel<SshEvent>();
  onEvent.onmessage = handlers.onEvent;
  const onDisplay = new Channel<RdpDisplayEvent>();
  onDisplay.onmessage = handlers.onDisplay;
  return invoke("rdp_connect", { hostId, width, height, onEvent, onDisplay, onFrame: rawChannel(handlers.onFrame) });
}

export const rdpInput = (id: number, ops: RdpInput[]): Promise<void> => invoke("rdp_input", { id, ops });
/** `false`: the server cannot resize live (no display control channel); reconnect to resize. */
export const rdpResize = (id: number, width: number, height: number): Promise<boolean> => invoke("rdp_resize", { id, width, height });
export const rdpClose = (id: number): Promise<void> => invoke("rdp_close", { id });
/** Re-announces the local clipboard to the remote desktop. */
export const rdpClipboardSync = (id: number): Promise<void> => invoke("rdp_clipboard_sync", { id });
/** Saves the files copied on the remote into `dir` (Downloads when omitted). */
export const rdpClipboardSave = (id: number, dir?: string): Promise<void> => invoke("rdp_clipboard_save", { id, dir: dir ?? null });
export const rdpClipboardCancel = (id: number): Promise<void> => invoke("rdp_clipboard_cancel", { id });

// AI assistant

export type AiProvider = "claudeCode" | "anthropic" | "openai" | "ollama" | "compatible";
export type AiEffort = "low" | "medium" | "high";

export interface AiSettings {
  provider: AiProvider;
  model: string;
  /** Empty: the provider's default. */
  baseUrl: string;
  effort: AiEffort;
}

export interface AiSettingsView {
  settings: AiSettings;
  hasKey: boolean;
  defaultBaseUrl: string;
  /** Whether the user has set the assistant up (else these are the defaults). */
  saved: boolean;
  /** The Claude Code CLI found on this computer, if any. */
  claudeCode: string | null;
}

/** A picture sent with a message (a remote desktop screenshot), base64 without the data: prefix. */
export interface AiImage {
  mediaType: "image/jpeg" | "image/png";
  data: string;
}

export interface AiChatMessage {
  role: "user" | "assistant";
  content: string;
  images?: AiImage[];
}

export type AiEvent =
  | { type: "model"; model: string }
  | { type: "thinking" }
  | { type: "text"; text: string }
  | { type: "done"; stopReason: string | null }
  | { type: "error"; message: string };

export const aiSettingsGet = (): Promise<AiSettingsView> => invoke("ai_settings_get");
export const aiSettingsSave = (settings: AiSettings, apiKey: SecretChange): Promise<AiSettingsView> =>
  invoke("ai_settings_save", { settings, apiKey });
/** Models offered by the provider; `apiKey` lets the form list them before the key is saved. */
export const aiModels = (settings: AiSettings, apiKey?: string): Promise<string[]> =>
  invoke("ai_models", { settings, apiKey: apiKey || null });

/**
 * Starts a reply; events stream to `onEvent` until "done" or "error". Returns the request id.
 * `desktop`: the agent may also use the remote desktop's mouse and keyboard.
 */
export function aiChat(messages: AiChatMessage[], agent: boolean, desktop: boolean, onEvent: (event: AiEvent) => void): Promise<number> {
  const channel = new Channel<AiEvent>();
  channel.onmessage = onEvent;
  return invoke("ai_chat", { messages, agent, desktop, onEvent: channel });
}

/** What a command run by the agent printed (the end of it), and how it ended. */
export interface ExecOutput {
  stdout: string;
  stderr: string;
  exitCode: number | null;
  timedOut: boolean;
}

/** Runs a command on an SSH session's server, on its own channel (not in the user's shell). */
export const sshExec = (id: number, command: string, timeoutSecs: number): Promise<ExecOutput> =>
  invoke("ssh_exec", { id, command, timeoutSecs });
// Connections without a terminal, for tools that run commands on a server (Docker)

/** Connects to a saved host; events as for a terminal (prompts, then `connected` or `closed`). */
export function remoteOpen(hostId: string, onEvent: (event: SshEvent) => void): Promise<number> {
  const channel = new Channel<SshEvent>();
  channel.onmessage = onEvent;
  return invoke("remote_open", { hostId, onEvent: channel });
}
/** Runs a command and returns all it printed (up to 8 MB). */
export const remoteExec = (id: number, command: string, timeoutSecs = 60): Promise<ExecOutput> => invoke("remote_exec", { id, command, timeoutSecs });
export const remoteClose = (id: number): Promise<void> => invoke("remote_close", { id });

/** Runs a command on this computer. */
export const localExec = (command: string, timeoutSecs: number): Promise<ExecOutput> => invoke("local_exec", { command, timeoutSecs });

export const aiCancel = (id: number): Promise<void> => invoke("ai_cancel", { id });

// Importing connections from other programs

export type ImportFormat = "sshConfig" | "putty" | "fileZilla" | "rdpFile" | "mobaXterm" | "royalTs";

export interface ImportedHost {
  name: string;
  hostname: string;
  port: number;
  username: string;
  protocol: Protocol;
  /** Folder path inside the source program. */
  folder: string | null;
  keyPath: string | null;
  color: string | null;
  /** A password comes along (it goes to the keychain, never to the UI). */
  hasPassword: boolean;
  /** The same connection is already saved. */
  duplicate: boolean;
}

export interface ImportPreview {
  token: number;
  format: ImportFormat;
  source: string;
  hosts: ImportedHost[];
  warnings: string[];
}

export interface DetectedImport {
  format: ImportFormat;
  path: string;
  count: number;
}

export const importDetect = (): Promise<DetectedImport[]> => invoke("import_detect");
export const importRead = (paths: string[]): Promise<ImportPreview> => invoke("import_read", { paths });
/** Saves the chosen hosts (indexes into the preview) inside `into` (null: top level). */
export const importApply = (token: number, selected: number[], into: string | null): Promise<HostsData> =>
  invoke("import_apply", { token, selected, into });

// The user's Hostealo account

export interface HostealoAccount {
  name: string | null;
  email: string | null;
}

export interface HostealoSyncResult {
  added: number;
  updated: number;
  unchanged: number;
  /** Servers still being set up (no address yet). */
  pending: number;
  data: HostsData;
}

export type PowerAction = "start" | "stop" | "restart";

export const hostealoAccount = (): Promise<HostealoAccount | null> => invoke("hostealo_account");
export const hostealoConnect = (apiKey: string): Promise<HostealoAccount> => invoke("hostealo_connect", { apiKey });
export const hostealoDisconnect = (): Promise<void> => invoke("hostealo_disconnect");
export const hostealoSync = (): Promise<HostealoSyncResult> => invoke("hostealo_sync");
export const hostealoPower = (serviceId: string, action: PowerAction): Promise<string> => invoke("hostealo_power", { serviceId, action });
/** Gives a Hostealo VPS a new root password and keeps it in the keychain for its host. */
export const hostealoResetPassword = (hostId: string): Promise<void> => invoke("hostealo_reset_password", { hostId });

// This computer's files (the local side of the file browser)

export interface LocalListing {
  path: string;
  /** `null` at the root of a drive. */
  parent: string | null;
  entries: SftpEntry[];
}

export const localHome = (): Promise<string> => invoke("local_home");
export const localList = (path: string): Promise<LocalListing> => invoke("local_list", { path });
export const localMkdir = (dir: string, name: string): Promise<void> => invoke("local_mkdir", { dir, name });
export const localRename = (path: string, name: string): Promise<void> => invoke("local_rename", { path, name });
export const localDelete = (path: string): Promise<void> => invoke("local_delete", { path });

// Syncing with the user's other devices over the local network

export interface SyncSummary {
  added: number;
  updated: number;
  deleted: number;
  secrets: number;
}

export type SyncEvent =
  | { type: "listening"; device: string; addresses: string[]; pairing: string }
  | { type: "found"; id: string; name: string; address: string }
  | { type: "lost"; id: string }
  | { type: "code"; peer: string; code: string; verified: boolean }
  | { type: "syncing" }
  | { type: "done"; summary: SyncSummary }
  | { type: "error"; message: string };

const syncChannel = (onEvent: (event: SyncEvent) => void) => {
  const channel = new Channel<SyncEvent>();
  channel.onmessage = onEvent;
  return channel;
};

/** Makes this device visible on the network until a device connects or it is cancelled. */
export const syncListen = (onEvent: (event: SyncEvent) => void): Promise<number> => invoke("sync_listen", { onEvent: syncChannel(onEvent) });
/** Lists the devices waiting on the network. */
export const syncBrowse = (onEvent: (event: SyncEvent) => void): Promise<number> => invoke("sync_browse", { onEvent: syncChannel(onEvent) });
/** Connects to a device: `ip:port` or a pairing link (from the QR code). */
export const syncConnect = (address: string, onEvent: (event: SyncEvent) => void): Promise<number> =>
  invoke("sync_connect", { address, onEvent: syncChannel(onEvent) });
/** The user compared the codes. */
export const syncConfirm = (id: number, accepted: boolean, includeSecrets: boolean): Promise<void> =>
  invoke("sync_confirm", { id, accepted, includeSecrets });
export const syncCancel = (id: number): Promise<void> => invoke("sync_cancel", { id });
/** The pairing link the app was opened with (the QR code scanned with the camera), once. */
export const syncOpenedLink = (): Promise<string | null> => invoke("sync_opened_link");

export interface BackupSummary {
  hosts: number;
  secrets: number;
}

/** Shortest passphrase the backend accepts for a backup. */
export const MIN_BACKUP_PASSPHRASE = 8;
/** Saves every host to a backup file encrypted with the passphrase. */
export const syncExport = (path: string, passphrase: string, includeSecrets: boolean): Promise<BackupSummary> =>
  invoke("sync_export", { path, passphrase, includeSecrets });
/** Merges a backup file into this device. */
export const syncImport = (path: string, passphrase: string): Promise<SyncSummary> => invoke("sync_import", { path, passphrase });

/** The system share sheet for local files (iPhone and iPad only). */
export const shareFiles = (paths: string[]): Promise<void> => invoke("share_files", { paths });

/** Asks the owner for Face ID, Touch ID or the device password; rejects with "cancelled". */
export const deviceUnlock = (reason: string): Promise<void> => invoke("device_unlock", { reason });
