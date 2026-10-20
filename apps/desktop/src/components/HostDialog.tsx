import { useEffect, useId, useRef, useState, type FormEvent } from "react";
import { useTranslation } from "react-i18next";
import { Ban, Check, Copy, Eye, EyeOff, FolderOpen, KeyRound, LoaderCircle, Server } from "lucide-react";
import { open } from "@tauri-apps/plugin-dialog";
import { homeDir, join } from "@tauri-apps/api/path";
import { cn } from "@hostealo/ui/cn";
import { DEFAULT_HOST_COLOR, HOST_COLORS, hostColorHex, type HostColorSetting } from "@hostealo/ui/host-colors";

import { hostsRevealSecret, sshKeysList, type AuthMethod, type Host, type Protocol, type SecretChange, type SshKeyInfo } from "../lib/ipc";
import { useResolvedTheme } from "../hooks/usePreferences";
import { useHosts } from "../store/hosts";
import { Button } from "./ui/Button";
import { CopyButton } from "./ui/CopyButton";
import { Field, Input, Segmented } from "./ui/Form";
import { Modal } from "./ui/Modal";

type AuthType = AuthMethod["type"];

const DEFAULT_KEY = "~/.ssh/id_ed25519";
/** Turns on Windows' own OpenSSH server (Windows Server 2019+, Windows 10/11); run as administrator. */
const OPENSSH_SETUP =
  "Add-WindowsCapability -Online -Name OpenSSH.Server~~~~0.0.1.0; Set-Service sshd -StartupType Automatic; Start-Service sshd";
const DEFAULT_PORTS: Record<Protocol, number> = { ssh: 22, ftp: 21, ftps: 21, rdp: 3389 };

interface HostDialogProps {
  /** Host to edit; a new host is created when omitted. */
  host?: Host;
  /** Folder for a new host. */
  defaultFolder?: string | null;
  onClose: () => void;
  onSaved: (host: Host, connect: boolean) => void;
}

export function HostDialog({ host, defaultFolder, onClose, onSaved }: HostDialogProps) {
  const { t } = useTranslation();
  const save = useHosts((s) => s.save);
  const folders = useHosts((s) => s.folders);
  const folderListId = useId();

  const [name, setName] = useState(host?.name ?? "");
  const [hostname, setHostname] = useState(host?.hostname ?? "");
  const [protocol, setProtocol] = useState<Protocol>(host?.protocol ?? "ssh");
  const [port, setPort] = useState(String(host?.port ?? 22));
  const [username, setUsername] = useState(host?.username ?? "");
  const [sharedFolder, setSharedFolder] = useState(host ? (host.rdpSharedFolder ?? "") : "~/Downloads");
  const [commandsHost, setCommandsHost] = useState(host?.rdpCommandsHost ?? "");
  const sshHosts = useHosts((s) => s.hosts).filter((h) => h.protocol === "ssh" && h.id !== host?.id);
  const [folder, setFolder] = useState(host ? (host.folder ?? "") : (defaultFolder ?? ""));
  const [color, setColor] = useState<HostColorSetting>((host?.color as HostColorSetting | null) ?? DEFAULT_HOST_COLOR);
  const [authType, setAuthType] = useState<AuthType>(host?.auth.type ?? "password");
  // FTP only knows passwords; the SSH choice is kept in case the user switches back.
  const effectiveAuth: AuthType = protocol === "ssh" ? authType : "password";

  const changeProtocol = (next: Protocol) => {
    // Follow the default port unless the user typed a custom one.
    if (Number(port) === DEFAULT_PORTS[protocol]) setPort(String(DEFAULT_PORTS[next]));
    setProtocol(next);
  };
  const [keyPath, setKeyPath] = useState(host?.auth.type === "key" ? host.auth.keyPath : DEFAULT_KEY);
  const [password, setPassword] = useState(secretState(host?.hasPassword));
  const [passphrase, setPassphrase] = useState(secretState(host?.hasPassphrase));
  const [errors, setErrors] = useState<Record<string, string>>({});
  const [submitError, setSubmitError] = useState<string | null>(null);
  const [saving, setSaving] = useState(false);
  const [localKeys, setLocalKeys] = useState<SshKeyInfo[]>([]);
  const keyListId = useId();

  // Suggest the keys found in ~/.ssh for the key path field.
  useEffect(() => {
    if (authType === "key" && localKeys.length === 0) void sshKeysList().then(setLocalKeys).catch(() => {});
  }, [authType, localKeys.length]);

  // Accept "user@host:port" pasted into the host field.
  const splitTarget = () => {
    const match = /^(?:([^@\s]+)@)?([^:@\s]+)(?::(\d{1,5}))?$/.exec(hostname.trim());
    if (!match || (!match[1] && !match[3])) return;
    setHostname(match[2]);
    if (match[1]) setUsername(match[1]);
    if (match[3]) setPort(match[3]);
  };

  const browseSharedFolder = async () => {
    const selected = await open({ directory: true, multiple: false, defaultPath: await homeDir() });
    if (typeof selected === "string") setSharedFolder(selected);
  };

  const browseKey = async () => {
    const selected = await open({ multiple: false, directory: false, defaultPath: await join(await homeDir(), ".ssh") });
    if (typeof selected === "string") setKeyPath(selected);
  };

  const submit = async (connect: boolean, event?: FormEvent) => {
    event?.preventDefault();
    const portNumber = Number(port);
    const found: Record<string, string> = {};
    if (!hostname.trim()) found.hostname = t("host.required");
    if (!username.trim()) found.username = t("host.required");
    if (!Number.isInteger(portNumber) || portNumber < 1 || portNumber > 65535) found.port = t("host.invalidPort");
    if (effectiveAuth === "key" && !keyPath.trim()) found.keyPath = t("host.required");
    setErrors(found);
    if (Object.keys(found).length > 0) return;

    const auth: AuthMethod =
      effectiveAuth === "key"
        ? { type: "key", keyPath: keyPath.trim() }
        : effectiveAuth === "agent"
          ? { type: "agent" }
          : { type: "password" };
    // Secrets that no longer apply to the chosen method are removed from the keychain.
    const passwordChange = effectiveAuth === "password" ? toChange(password) : clearIfSaved(host?.hasPassword);
    const passphraseChange = effectiveAuth === "key" ? toChange(passphrase) : clearIfSaved(host?.hasPassphrase);

    setSaving(true);
    setSubmitError(null);
    try {
      const saved = await save(
        {
          id: host?.id ?? "",
          name: name.trim(),
          hostname: hostname.trim(),
          port: portNumber,
          username: username.trim(),
          protocol,
          auth,
          color,
          folder: folder.trim() || null,
          rdpSharedFolder: protocol === "rdp" ? sharedFolder.trim() || null : (host?.rdpSharedFolder ?? null),
          rdpCommandsHost: protocol === "rdp" ? commandsHost || null : (host?.rdpCommandsHost ?? null),
          hasPassword: false,
          hasPassphrase: false,
        },
        passwordChange,
        passphraseChange,
      );
      onSaved(saved, connect);
    } catch (error) {
      setSubmitError(String(error));
      setSaving(false);
    }
  };

  return (
    <Modal
      title={host ? t("host.editTitle") : t("host.newTitle")}
      icon={<Server className="size-4" />}
      onClose={onClose}
      className="max-w-lg"
      footer={
        <>
          <Button onClick={onClose}>{t("common.cancel")}</Button>
          <Button onClick={() => void submit(false)} disabled={saving}>
            {t("common.save")}
          </Button>
          <Button variant="primary" onClick={() => void submit(true)} disabled={saving}>
            {t("host.saveAndConnect")}
          </Button>
        </>
      }
    >
      <form className="flex flex-col gap-4" onSubmit={(e) => void submit(true, e)}>
        <Field label={t("host.name")} hint={t("host.nameHint")}>
          {(id) => <Input id={id} value={name} onChange={(e) => setName(e.target.value)} placeholder={hostname || t("host.namePlaceholder")} />}
        </Field>

        <div className="flex flex-col gap-2">
          <span className="text-sm font-medium text-fg">{t("host.protocol")}</span>
          <Segmented<Protocol>
            value={protocol}
            onChange={changeProtocol}
            options={[
              { value: "ssh", label: "SSH / SFTP" },
              { value: "ftp", label: "FTP" },
              { value: "ftps", label: "FTPS" },
              { value: "rdp", label: "RDP" },
            ]}
          />
          {protocol === "ftp" && <p className="text-xs text-amber">{t("host.ftpInsecure")}</p>}
          {protocol === "ftps" && <p className="text-xs text-fg-muted">{t("host.ftpsHint")}</p>}
          {protocol === "rdp" && <p className="text-xs text-fg-muted">{t("host.rdpHint")}</p>}
        </div>

        <Field label={t("host.folder")} hint={t("host.folderHint")}>
          {(id) => (
            <>
              <Input id={id} list={folderListId} value={folder} onChange={(e) => setFolder(e.target.value)} placeholder={t("host.noFolder")} />
              <datalist id={folderListId}>
                {folders.map((f) => (
                  <option key={f} value={f} />
                ))}
              </datalist>
            </>
          )}
        </Field>

        <div className="grid grid-cols-[1fr_6rem] gap-3">
          <Field label={t("host.hostname")} error={errors.hostname}>
            {(id) => (
              <Input
                id={id}
                value={hostname}
                onChange={(e) => setHostname(e.target.value)}
                onBlur={splitTarget}
                placeholder="203.0.113.10"
              />
            )}
          </Field>
          <Field label={t("host.port")} error={errors.port}>
            {(id) => <Input id={id} value={port} onChange={(e) => setPort(e.target.value)} inputMode="numeric" />}
          </Field>
        </div>

        <Field label={t("host.username")} error={errors.username}>
          {(id) => <Input id={id} value={username} onChange={(e) => setUsername(e.target.value)} placeholder="root" />}
        </Field>

        <div className="flex flex-col gap-2">
          <span className="text-sm font-medium text-fg">{t("host.color")}</span>
          <ColorPicker value={color} onChange={setColor} />
          <p className="text-xs text-fg-muted">{t("host.colorHint")}</p>
        </div>

        {protocol === "ssh" && (
          <div className="flex flex-col gap-2">
            <span className="text-sm font-medium text-fg">{t("host.auth")}</span>
            <Segmented<AuthType>
              value={authType}
              onChange={setAuthType}
              options={[
                { value: "password", label: t("host.authPassword") },
                { value: "key", label: t("host.authKey") },
                { value: "agent", label: t("host.authAgent") },
              ]}
            />
          </div>
        )}

        {protocol === "rdp" && (
          <Field label={t("host.sharedFolder")} hint={t("host.sharedFolderHint")}>
            {(id) => (
              <div className="flex gap-2">
                <Input id={id} value={sharedFolder} onChange={(e) => setSharedFolder(e.target.value)} placeholder={t("host.sharedFolderNone")} className="font-mono text-[13px]" />
                <Button onClick={() => void browseSharedFolder()} title={t("host.browse")} aria-label={t("host.browse")}>
                  <FolderOpen className="size-4" />
                </Button>
              </div>
            )}
          </Field>
        )}

        {protocol === "rdp" && (
          <Field
            label={t("host.commandsHost")}
            hint={
              <>
                {t("host.commandsHostHint")}
                <span className="mt-1.5 flex items-start gap-2">
                  <code className="min-w-0 flex-1 rounded-btn bg-deep px-2 py-1.5 font-mono text-[11.5px] break-all text-fg-secondary">{OPENSSH_SETUP}</code>
                  <CopyButton text={OPENSSH_SETUP} label={t("ai.copy")} variant="ghost" />
                </span>
              </>
            }
          >
            {(id) => (
              <select
                id={id}
                value={commandsHost}
                onChange={(e) => setCommandsHost(e.target.value)}
                className="h-9 touch:h-11 w-full min-w-0 cursor-pointer rounded-btn border border-input-border bg-input px-2.5 text-sm text-fg shadow-sm outline-none focus:border-fg-muted focus:ring-4 focus:ring-ring"
              >
                <option value="">{t("host.commandsHostNone")}</option>
                {sshHosts.map((h) => (
                  <option key={h.id} value={h.id}>
                    {h.name} ({h.username}@{h.hostname})
                  </option>
                ))}
              </select>
            )}
          </Field>
        )}

        {effectiveAuth === "password" && (
          <SecretField
            label={t("host.password")}
            hint={t("host.passwordHint")}
            state={password}
            onChange={setPassword}
            reveal={host && (() => hostsRevealSecret(host.id, "password", t("host.revealReason", { name: host.name })))}
          />
        )}

        {effectiveAuth === "key" && (
          <>
            <Field label={t("host.keyPath")} error={errors.keyPath}>
              {(id) => (
                <div className="flex gap-2">
                  <Input id={id} list={keyListId} value={keyPath} onChange={(e) => setKeyPath(e.target.value)} className="font-mono text-[13px]" />
                  <datalist id={keyListId}>
                    {localKeys.map((key) => (
                      <option key={key.path} value={key.path}>
                        {key.name} · {key.keyType}
                      </option>
                    ))}
                  </datalist>
                  <Button onClick={() => void browseKey()} title={t("host.browse")} aria-label={t("host.browse")}>
                    <FolderOpen className="size-4" />
                  </Button>
                </div>
              )}
            </Field>
            <SecretField
              label={t("host.passphrase")}
              hint={t("host.passphraseHint")}
              state={passphrase}
              onChange={setPassphrase}
              reveal={host && (() => hostsRevealSecret(host.id, "passphrase", t("host.revealReason", { name: host.name })))}
            />
          </>
        )}

        {effectiveAuth === "agent" && (
          <p className="flex gap-2 rounded-card-deep border border-border bg-deep p-3 text-xs leading-relaxed text-fg-secondary">
            <KeyRound className="mt-0.5 size-3.5 shrink-0 text-accent" />
            {t("host.agentHint")}
          </p>
        )}

        {submitError && <p className="text-sm text-red">{submitError}</p>}
        {/* Enter in any field submits ("save and connect"). */}
        <button type="submit" hidden />
      </form>
    </Modal>
  );
}

interface SecretState {
  saved: boolean;
  value: string;
  cleared: boolean;
}

const secretState = (saved = false): SecretState => ({ saved, value: "", cleared: false });

function toChange({ value, cleared }: SecretState): SecretChange {
  if (value) return { action: "set", value };
  return cleared ? { action: "clear" } : { action: "keep" };
}

const clearIfSaved = (saved?: boolean): SecretChange => (saved ? { action: "clear" } : { action: "keep" });

/** How long a saved secret stays visible after "show". */
const REVEAL_MS = 30_000;

function SecretField({
  label,
  hint,
  state,
  onChange,
  reveal,
}: {
  label: string;
  hint: string;
  state: SecretState;
  onChange: (state: SecretState) => void;
  /** Reads the saved secret (with the OS confirmation); absent for a new host. */
  reveal?: () => Promise<string>;
}) {
  const { t } = useTranslation();
  const stored = state.saved && !state.cleared;
  // What is typed can be shown as is; the saved one comes from the keychain, for a while.
  const [visible, setVisible] = useState(false);
  const [revealed, setRevealed] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [copied, setCopied] = useState(false);
  const hideTimer = useRef<ReturnType<typeof setTimeout>>(undefined);

  useEffect(() => () => clearTimeout(hideTimer.current), []);

  const hide = () => {
    clearTimeout(hideTimer.current);
    setRevealed(null);
    setVisible(false);
  };

  const toggle = async () => {
    setError(null);
    if (visible || revealed !== null) return hide();
    if (state.value || !stored || !reveal) return setVisible(true);
    setBusy(true);
    try {
      setRevealed(await reveal());
      setVisible(true);
      hideTimer.current = setTimeout(hide, REVEAL_MS);
    } catch (e) {
      if (String(e) !== "cancelled") setError(String(e));
    } finally {
      setBusy(false);
    }
  };

  const shown = revealed !== null && !state.value ? revealed : state.value;
  const canToggle = Boolean(state.value) || (stored && Boolean(reveal));

  return (
    <Field label={label} hint={revealed !== null ? t("host.revealedHint") : hint} error={error ?? undefined}>
      {(id) => (
        <div className="flex gap-2">
          <div className="relative min-w-0 flex-1">
            <Input
              id={id}
              type={visible ? "text" : "password"}
              value={shown}
              onChange={(e) => {
                // Typing over a shown secret starts a new one.
                setRevealed(null);
                onChange({ ...state, value: e.target.value });
              }}
              placeholder={stored ? t("host.savedInKeychain") : t("host.optional")}
              className={cn(canToggle && "pr-9", visible && "font-mono text-[13px]")}
            />
            {canToggle && (
              <button
                type="button"
                onClick={() => void toggle()}
                disabled={busy}
                title={visible ? t("host.hideSecret") : t("host.showSecret")}
                aria-label={visible ? t("host.hideSecret") : t("host.showSecret")}
                className="absolute top-1/2 right-1.5 grid size-7 -translate-y-1/2 cursor-pointer place-items-center rounded-md text-fg-muted hover:bg-muted hover:text-fg"
              >
                {busy ? <LoaderCircle className="size-4 animate-spin" /> : visible ? <EyeOff className="size-4" /> : <Eye className="size-4" />}
              </button>
            )}
          </div>
          {revealed !== null && (
            <Button
              onClick={() => {
                void navigator.clipboard.writeText(revealed);
                setCopied(true);
                setTimeout(() => setCopied(false), 1500);
              }}
              title={t("host.copySecret")}
              aria-label={t("host.copySecret")}
            >
              {copied ? <Check className="size-4 text-green" /> : <Copy className="size-4" />}
            </Button>
          )}
          {stored && (
            <Button
              onClick={() => {
                hide();
                onChange({ ...state, value: "", cleared: true });
              }}
            >
              {t("host.forget")}
            </Button>
          )}
        </div>
      )}
    </Field>
  );
}

function ColorPicker({ value, onChange }: { value: HostColorSetting; onChange: (color: HostColorSetting) => void }) {
  const { t } = useTranslation();
  const theme = useResolvedTheme();
  const options: HostColorSetting[] = ["none", ...(Object.keys(HOST_COLORS) as HostColorSetting[])];
  return (
    <div role="radiogroup" aria-label={t("host.color")} className="flex flex-wrap gap-2">
      {options.map((option) => {
        const selected = option === value;
        const hex = option === "none" ? undefined : hostColorHex(option, theme);
        return (
          <button
            key={option}
            type="button"
            role="radio"
            aria-checked={selected}
            title={t(`host.colors.${option}`)}
            aria-label={t(`host.colors.${option}`)}
            onClick={() => onChange(option)}
            style={hex ? { backgroundColor: hex } : undefined}
            className={cn(
              "grid size-7 cursor-pointer place-items-center rounded-full border transition-transform hover:scale-110",
              hex ? "border-transparent text-white" : "border-border bg-deep text-fg-muted",
              selected && "ring-2 ring-fg ring-offset-2 ring-offset-card",
            )}
          >
            {option === "none" ? <Ban className="size-3.5" /> : selected && <Check className="size-3.5" strokeWidth={3} />}
          </button>
        );
      })}
    </div>
  );
}
