// Website demo: the real app in the browser on a pretend backend. Servers answer from a simulated
// shell (shell.ts), the assistant has prepared answers, and nothing ever leaves the page.
// Built on its own with `npm run build:demo` (vite.demo.config.ts).

import { isIOS } from "../lib/format";
import { mockIPC, mockWindows } from "@tauri-apps/api/mocks";

import type { Host, HostsData, Snippet } from "../lib/ipc";
import { bytes, fakeDesktop, FOLDERS, host, HOSTS, KEYS, KNOWN, later, send, SFTP, snapshot, SNIPPETS, snippetStore, SYSTEMS, dockerExec, proxmoxExec } from "./fixtures";
import { FakeShell } from "./shell";

const params = new URLSearchParams(location.search);
// Spanish by default, like hostealo.es; ?lang=en for English (the README's recordings).
const language = params.get("lang") === "en" ? "en" : "es";
const EN = language === "en";

/** The sample data in English (folder, command and server names). */
const NAMES_EN: Record<string, string> = {
  Clientes: "Clients",
  Producción: "Production",
  "s-disk": "Disk space",
  "s-docker": "Containers",
  "s-logs": "nginx logs",
  "s-reload": "Reload nginx",
  "s-update": "Update packages",
  "s-reboot": "Reboot the server",
  "vps-tienda": "vps-shop",
  "vps-correo": "vps-mail",
  "dedicado-01": "dedicated-01",
};
const folderName = (path: string | null) => (path && EN ? path.split("/").map((part) => NAMES_EN[part] ?? part).join("/") : path);
const name = (value: string) => (EN ? (NAMES_EN[value] ?? value) : value);

const data: HostsData = {
  hosts: HOSTS.map((h) => ({ ...h, folder: folderName(h.folder) })),
  folders: FOLDERS.map((f) => folderName(f)!),
};
const shells = new Map<number, FakeShell>();
const sessionHost = new Map<number, Host>();
const timers = new Map<number, ReturnType<typeof setInterval>>();
let nextId = 1;
const dockerState = new Map<string, string>();
/** The remote desktop's channels, to answer resizes. */
let rdpDisplay: unknown = null;
let rdpFrame: unknown = null;

const snippets = snippetStore(SNIPPETS.map((snippet) => ({ ...snippet, name: EN ? (NAMES_EN[snippet.id] ?? snippet.name) : snippet.name })));
const hostsData = (): HostsData => ({ hosts: data.hosts, folders: data.folders, snippets: snippets.list() });

function openShell(id: number, onData: unknown, target: Host | null) {
  const system = target
    ? { hostname: target.name, user: target.username || "root", osName: SYSTEMS[target.id]?.[1] ?? "Ubuntu 24.04.1 LTS", local: false }
    : { hostname: "MacBook-Pro", user: "demo", osName: "macOS 15.6", local: true };
  const shell = new FakeShell({ ...system, lang: language, quiet: params.has("clean") }, (text) => send(onData, bytes(text)));
  shells.set(id, shell);
  shell.start();
}

// ── The assistant ────────────────────────────────────────

interface ChatMessage {
  role: "user" | "assistant";
  content: string;
}

/** Commands the agent runs for a request, then what it says at the end. */
const PLANS: { match: RegExp; steps: string[]; done: string }[] = [
  {
    match: /nginx|instal|install/i,
    steps: [
      "Es Ubuntu 24.04, así que lo instalo con apt sin preguntas interactivas:\n\n```run\nDEBIAN_FRONTEND=noninteractive apt-get install -y nginx\n```",
      "Instalado. Compruebo que responde en el puerto 80:\n\n```run\ncurl -sI http://localhost | head -2\n```",
    ],
    done: "Listo: **nginx 1.24** está instalado y responde con `200 OK` en el puerto 80. El sitio por defecto está en `/var/www/html`.",
  },
  {
    match: /disco|espacio|disk|lleno|space/i,
    steps: [
      "Miro primero cómo está el disco:\n\n```run\ndf -h /\n```",
      "Hay sitio de sobra (38 % usado). Veo qué ocupa más en `/var`, por si acaso:\n\n```run\ndu -sh /var/* 2>/dev/null | sort -h | tail -3\n```",
    ],
    done: "El disco está bien: **46 GB libres** de 79 GB. Lo que más ocupa es `/var/lib/docker` (14 GB, las imágenes de los contenedores) y `/var/log` (1,2 GB). Si quieres ganar espacio, `docker image prune` liberaría unas imágenes antiguas.",
  },
  {
    match: /memoria|ram|memory|lento|slow|carga|load/i,
    steps: ["Compruebo memoria y carga:\n\n```run\nfree -h && uptime\n```"],
    done: "Todo normal: **4,6 GB de RAM disponibles** de 7,8 GB, sin usar swap, y una carga de 0,42 con 4 núcleos. Si va lento, no es por el servidor; miraría la aplicación o la base de datos.",
  },
  {
    match: /.*/,
    steps: ["Echo un vistazo general al servidor:\n\n```run\nuptime && df -h / && systemctl --failed --no-legend\n```"],
    done: "El servidor está sano: lleva **12 días encendido**, el disco al 38 % y **ningún servicio caído**. (En la demo las respuestas están preparadas; en la app responde Claude de verdad.)",
  },
];

const EXEC: [RegExp, string][] = [
  [/apt-get install/, "Reading package lists...\nBuilding dependency tree...\nThe following NEW packages will be installed:\n  nginx nginx-common\nSetting up nginx (1.24.0-2ubuntu7) ...\n"],
  [/curl/, "HTTP/1.1 200 OK\nServer: nginx/1.24.0 (Ubuntu)\n"],
  [/^df/, "Filesystem      Size  Used Avail Use% Mounted on\n/dev/vda1        79G   30G   46G  38% /\n"],
  [/^du/, "320M\t/var/cache\n1.2G\t/var/log\n14G\t/var/lib\n"],
  [/^free/, "               total        used        free      shared  buff/cache   available\nMem:           7.8Gi       2.9Gi       1.2Gi        92Mi       3.7Gi       4.6Gi\nSwap:          2.0Gi          0B       2.0Gi\n 12:41:07 up 12 days,  4:02,  1 user,  load average: 0.42, 0.38, 0.35\n"],
  [/^uptime/, " 12:41:07 up 12 days,  4:02,  1 user,  load average: 0.42, 0.38, 0.35\nFilesystem      Size  Used Avail Use% Mounted on\n/dev/vda1        79G   30G   46G  38% /\n"],
];

const CHAT: [RegExp, string][] = [
  [
    /tr[aá]fico|traffic|tcpdump/i,
    "El tráfico que ves es casi todo **HTTPS (443)** hacia `203.0.113.10`; las peticiones a `:22` son tu propia sesión SSH. No hay nada raro.",
  ],
  [
    /login|ssh|ataque|attack|failed|seguridad/i,
    "En `auth.log` hay intentos fallidos de entrar como **root con contraseña** desde `192.0.2.66`. Como el servidor solo acepta claves, no pueden entrar, pero conviene bloquear esa IP:\n\n```bash\nufw deny from 192.0.2.66\n```\n\nY si activas **Agente**, lo hago yo con tu permiso.",
  ],
];

const CHAT_DEFAULT =
  "En esta demo el asistente tiene respuestas preparadas. En la app responde **Claude** de verdad, con tu clave de API o con tu suscripción a través de Claude Code, y ve lo que pasa en tu terminal.\n\nPrueba a activar **Agente** y pedirle *instala nginx* o *¿cómo va el disco?*: propondrá los comandos y los ejecutará cuando los apruebes.";

function stream(onEvent: unknown, reply: string, thinking: boolean) {
  const words = reply.split(/(?<=\s)/);
  const start = thinking ? 700 : 400;
  later(100, () => send(onEvent, { type: "model", model: "claude-opus-5-5" }));
  if (thinking) later(150, () => send(onEvent, { type: "thinking" }));
  words.forEach((word, i) => later(start + i * 14, () => send(onEvent, { type: "text", text: word })));
  later(start + 100 + words.length * 14, () => send(onEvent, { type: "done", stopReason: "end_turn" }));
}

const PLANS_EN: typeof PLANS = [
  {
    match: /nginx|instal|install/i,
    steps: [
      "It runs Ubuntu 24.04, so I install it with apt, without interactive questions:\n\n```run\nDEBIAN_FRONTEND=noninteractive apt-get install -y nginx\n```",
      "Installed. Now I check that it answers on port 80:\n\n```run\ncurl -sI http://localhost | head -2\n```",
    ],
    done: "Done: **nginx 1.24** is installed and answers `200 OK` on port 80. The default site is in `/var/www/html`.",
  },
  {
    match: /disk|space|full|disco/i,
    steps: [
      "First, how the disk looks:\n\n```run\ndf -h /\n```",
      "Plenty of room (38 % used). Just in case, what takes the most in `/var`:\n\n```run\ndu -sh /var/* 2>/dev/null | sort -h | tail -3\n```",
    ],
    done: "The disk is fine: **46 GB free** of 79 GB. The biggest are `/var/lib/docker` (14 GB, container images) and `/var/log` (1.2 GB). `docker image prune` would free some old images.",
  },
  {
    match: /memory|ram|slow|load/i,
    steps: ["Checking memory and load:\n\n```run\nfree -h && uptime\n```"],
    done: "All normal: **4.6 GB of RAM available** of 7.8 GB, no swap in use, and a load of 0.42 on 4 cores. If it feels slow, the server is not the cause; I would look at the app or the database.",
  },
  {
    match: /.*/,
    steps: ["A general look at the server:\n\n```run\nuptime && df -h / && systemctl --failed --no-legend\n```"],
    done: "The server is healthy: **up 12 days**, disk at 38 % and **no failed services**. (In the demo the answers are prepared; in the app Claude answers for real.)",
  },
];

const CHAT_EN: typeof CHAT = [
  [/traffic|tcpdump/i, "Almost all the traffic you see is **HTTPS (443)** to `203.0.113.10`; the `:22` packets are your own SSH session. Nothing unusual."],
  [
    /login|ssh|attack|failed|security/i,
    "`auth.log` shows failed **root password** logins from `192.0.2.66`. The server only accepts keys, so they cannot get in, but blocking that address is worth it:\n\n```bash\nufw deny from 192.0.2.66\n```\n\nTurn on **Agent** and I will do it, with your permission.",
  ],
];

const CHAT_DEFAULT_EN =
  "In this demo the assistant has prepared answers. In the app **Claude** answers for real, with your API key or your subscription through Claude Code, and it sees what happens in your terminal.\n\nTurn on **Agent** and ask *install nginx* or *how is the disk?*: it proposes the commands and runs them when you approve.";

/** Docker sample names in English. */
function english<T extends { stdout: string } | null>(output: T): T {
  return EN && output ? { ...output, stdout: output.stdout.replaceAll("tienda", "shop") } : output;
}

function aiReply(messages: ChatMessage[], agent: boolean): string {
  // The request is the last message the user typed; command outputs come back as hidden turns.
  let start = messages.length - 1;
  const typed = (m: ChatMessage) => m.role === "user" && !m.content.startsWith("<command_output") && !m.content.startsWith("I declined");
  while (start > 0 && !typed(messages[start])) start--;
  // A question about the file open in the editor: a fixed version to apply.
  if ((messages[start]?.content ?? "").includes("File open in the editor:")) return fileReply();
  // Without the terminal output attached to the question.
  const request = (messages[start]?.content ?? "").replace(/<terminal_context>[\s\S]*<\/terminal_context>/, "");
  if (!agent) return (EN ? CHAT_EN : CHAT).find(([match]) => match.test(request))?.[1] ?? (EN ? CHAT_DEFAULT_EN : CHAT_DEFAULT);
  const plan = (EN ? PLANS_EN : PLANS).find((p) => p.match.test(request))!;
  const step = Math.floor((messages.length - 1 - start) / 2);
  return plan.steps[step] ?? plan.done;
}

/** The demo's answer about a file in the editor: what to change, and the whole file to apply. */
function fileReply(): string {
  const intro = EN
    ? "Two things to fix: `docker compose pull` fails silently if the registry is down, and the health check runs before the container is ready. The fixed script waits for it:"
    : "Dos cosas que arreglar: `docker compose pull` falla en silencio si el registro está caído, y la comprobación corre antes de que el contenedor esté listo. El script corregido espera:";
  return `${intro}

\`\`\`bash
#!/usr/bin/env bash
# Pulls the latest image and restarts the shop without downtime.
set -euo pipefail

APP_DIR=/srv/tienda
cd "$APP_DIR"

echo "Pulling $(date +%F)…"
docker compose pull --quiet web
docker compose up -d --no-deps --wait web

for attempt in 1 2 3 4 5; do
  if curl -fsS http://127.0.0.1:3000/health > /dev/null; then
    echo "Deployed ✓"
    exit 0
  fi
  sleep 2
done
echo "Health check failed" >&2
exit 1
\`\`\``;
}

// ── Files ─────────────────────────────────────────────────

function remoteListing(path: string) {
  if (path === "/root") return SFTP;
  const now = Math.floor(Date.now() / 1000);
  const names = path.endsWith("etc") ? ["nginx", "ssh", "hosts", "hostname", "os-release"] : ["index.html", "assets", "robots.txt"];
  return names.map((name, i) => {
    const isDir = !name.includes(".") && !["hosts", "hostname", "os-release"].includes(name);
    return { name, path: `${path}/${name}`, isDir, isSymlink: false, size: isDir ? 4096 : 900 + i * 731, modified: now - i * 86400, permissions: isDir ? 0o755 : 0o644, owner: "root" };
  });
}

/** What the editor shows for the demo's files. */
function demoText(path: string): { content: string; encoding: "utf-8"; modified: number } {
  const name = path.split("/").pop() ?? "";
  const modified = Math.floor(Date.now() / 1000) - 3600;
  if (path.includes("nginx") || name.endsWith(".conf")) {
    return {
      encoding: "utf-8",
      modified,
      content: `server {
    listen 80;
    server_name tienda.example.com;
    return 301 https://$host$request_uri;
}

server {
    listen 443 ssl http2;
    server_name tienda.example.com;

    ssl_certificate     /etc/letsencrypt/live/tienda.example.com/fullchain.pem;
    ssl_certificate_key /etc/letsencrypt/live/tienda.example.com/privkey.pem;

    # The shop runs in Docker on port 3000
    location / {
        proxy_pass http://127.0.0.1:3000;
        proxy_set_header Host $host;
        proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
    }

    location /static/ {
        root /var/www/tienda;
        expires 30d;
    }
}
`,
    };
  }
  if (name === "deploy.sh" || name === ".bashrc") {
    return {
      encoding: "utf-8",
      modified,
      content: `#!/usr/bin/env bash
# Pulls the latest image and restarts the shop without downtime.
set -euo pipefail

APP_DIR=/srv/tienda
cd "$APP_DIR"

echo "Pulling $(date +%F)…"
docker compose pull web
docker compose up -d --no-deps web

if curl -fsS http://127.0.0.1:3000/health > /dev/null; then
  echo "Deployed ✓"
else
  echo "Health check failed" >&2
  exit 1
fi
`,
    };
  }
  if (name === ".env") {
    return {
      encoding: "utf-8",
      modified,
      content: `APP_ENV=production
APP_URL=https://tienda.example.com
DB_HOST=tienda-db-1
DB_DATABASE=tienda
DB_USERNAME=tienda
DB_PASSWORD=Xk29-demo-only
REDIS_URL=redis://tienda-cache-1:6379
`,
    };
  }
  if (name === "os-release") {
    return {
      encoding: "utf-8",
      modified,
      content: `PRETTY_NAME="Ubuntu 24.04.1 LTS"
NAME="Ubuntu"
VERSION_ID="24.04"
VERSION="24.04.1 LTS (Noble Numbat)"
ID=ubuntu
ID_LIKE=debian
HOME_URL="https://www.ubuntu.com/"
`,
    };
  }
  return { encoding: "utf-8", modified, content: `${name}\n` };
}

function transfer(onProgress: unknown, name: string, total: number): Promise<null> {
  return new Promise((resolve) => {
    for (let step = 1; step <= 10; step++) {
      later(step * 120, () => {
        send(onProgress, { transferred: (total * step) / 10, total, filesDone: step === 10 ? 1 : 0, filesTotal: 1, current: name });
        if (step === 10) resolve(null);
      });
    }
  });
}

// ── The backend ───────────────────────────────────────────

/** What the import dialog finds in the "MobaXterm" file. */
const IMPORTABLE_ES = [
  { name: "web-02", hostname: "203.0.113.12", port: 22, username: "root", protocol: "ssh" as const, folder: "Producción", keyPath: null, color: null, hasPassword: false, duplicate: false },
  { name: "dc-01", hostname: "10.0.0.5", port: 3389, username: "ACME\\admin", protocol: "rdp" as const, folder: "Clientes/Acme", keyPath: null, color: null, hasPassword: false, duplicate: false },
  { name: "backup", hostname: "192.0.2.80", port: 2222, username: "backup", protocol: "ssh" as const, folder: null, keyPath: null, color: null, hasPassword: false, duplicate: false },
];
const IMPORTABLE = IMPORTABLE_ES.map((item) => ({ ...item, folder: folderName(item.folder) }));

let syncEvents: unknown = null;

mockWindows("main");
mockIPC(
  (cmd, args = {}) => {
    const a = args as Record<string, unknown>;
    switch (cmd) {
      case "hosts_list":
        return hostsData();
      case "hosts_save": {
        const saved = { ...(a.host as Host) };
        if (!saved.id) saved.id = `demo-${nextId++}`;
        saved.hasPassword = (a.password as { action: string }).action === "set" || saved.hasPassword;
        const index = data.hosts.findIndex((h) => h.id === saved.id);
        if (index >= 0) data.hosts[index] = { ...data.hosts[index], ...saved };
        else data.hosts.push(saved);
        if (saved.folder && !data.folders.includes(saved.folder)) data.folders.push(saved.folder);
        return saved;
      }
      case "hosts_delete":
        data.hosts = data.hosts.filter((h) => h.id !== a.id);
        return null;
      case "hosts_set_os":
        return null;
      case "snippets_save":
        return snippets.save(a.snippet as Snippet);
      case "snippets_delete":
        return snippets.remove(String(a.id));
      case "hosts_reveal_secret":
        return new Promise((resolve) => later(500, () => resolve("Pr0d-w3b-01!2026")));
      // Face ID / Touch ID: the demo has no owner to ask.
      case "device_unlock":
        return new Promise((resolve) => later(400, () => resolve(null)));
      case "share_files":
        return null;
      case "rdp_resize":
        send(rdpDisplay, { type: "size", width: a.width, height: a.height });
        later(50, () => send(rdpFrame, fakeDesktop(Number(a.width), Number(a.height))));
        return true;
      case "remote_open":
        later(400, () => send(a.onEvent, { type: "status", stage: "connected" }));
        return 31;
      case "remote_exec":
        return new Promise((resolve) => later(String(a.command).includes(" stats ") ? 600 : 150, () => resolve(english(proxmoxExec(String(a.command)) ?? dockerExec(String(a.command), dockerState)))));
      case "remote_close":
        return null;
      case "hostealo_reset_password":
        return new Promise((resolve) => later(800, () => resolve(null)));
      case "folders_create":
        if (!data.folders.includes(String(a.path))) data.folders.push(String(a.path));
        return hostsData();
      case "folders_rename": {
        const [from, to] = [String(a.from), String(a.to)];
        const move = (path: string) => (path === from || path.startsWith(from + "/") ? to + path.slice(from.length) : path);
        data.folders = data.folders.map(move);
        for (const h of data.hosts) if (h.folder) h.folder = move(h.folder);
        return hostsData();
      }
      case "folders_delete": {
        const path = String(a.path);
        const parent = path.includes("/") ? path.slice(0, path.lastIndexOf("/")) : null;
        data.folders = data.folders.filter((f) => f !== path).map((f) => (f.startsWith(path + "/") ? (parent ? parent + "/" : "") + f.slice(path.length + 1) : f));
        for (const h of data.hosts) if (h.folder === path) h.folder = parent;
        return hostsData();
      }

      case "monitor_local":
        return snapshot(false);
      case "pty_spawn": {
        const id = nextId++;
        later(50, () => openShell(id, a.onData, null));
        return id;
      }
      case "ssh_connect": {
        const id = nextId++;
        const target = data.hosts.find((h) => h.id === a.hostId) ?? host("x", "server", "203.0.113.99", null);
        sessionHost.set(id, target);
        send(a.onEvent, { type: "status", stage: "connecting" });
        later(250, () => send(a.onEvent, { type: "status", stage: "authenticating" }));
        later(550, () => {
          send(a.onEvent, { type: "status", stage: "connected" });
          openShell(id, a.onData, target);
          const tick = () => {
            const snap = snapshot(true);
            snap.hostname = target.name;
            snap.os = SYSTEMS[target.id]?.[1] ?? snap.os;
            // ?fulldisk: web-01's database disk nearly full, to show an alert.
            if (params.has("fulldisk") && target.id === "web") snap.disks[1].used = 187 * 2 ** 30;
            send(a.onMonitor, { type: "snapshot", snapshot: snap });
          };
          tick();
          timers.set(id, setInterval(tick, 2000));
        });
        return id;
      }
      case "pty_write":
      case "ssh_write":
        shells.get(Number(a.id))?.input(String(a.data));
        return null;
      case "ssh_close":
      case "pty_kill":
        clearInterval(timers.get(Number(a.id)));
        shells.delete(Number(a.id));
        return null;
      case "ssh_exec":
      case "local_exec": {
        const command = String(a.command);
        const stdout = EXEC.find(([match]) => match.test(command))?.[1] ?? "";
        return new Promise((resolve) => later(900, () => resolve({ stdout, stderr: "", exitCode: 0, timedOut: false })));
      }

      case "rdp_connect": {
        // The size asked for, as a server with live resizing gives it.
        const [width, height] = [Number(a.width), Number(a.height)];
        rdpDisplay = a.onDisplay;
        rdpFrame = a.onFrame;
        later(400, () => {
          send(a.onEvent, { type: "status", stage: "connected" });
          send(a.onDisplay, { type: "size", width, height });
          send(a.onFrame, fakeDesktop(width, height));
          // ?audio: show the sound indicator as playing (recordings).
          if (params.has("audio")) send(a.onDisplay, { type: "audio", playing: true, error: null });
        });
        return nextId++;
      }

      case "ai_settings_get":
      case "ai_settings_save":
        return {
          settings: { provider: "anthropic", model: "claude-opus-5-5", baseUrl: "", effort: "medium" },
          hasKey: true,
          defaultBaseUrl: "https://api.anthropic.com",
          saved: true,
          claudeCode: "/opt/homebrew/bin/claude",
        };
      case "ai_models":
        if ((a.settings as { provider: string }).provider === "claudeCode") return ["opus", "sonnet", "haiku"];
        return ["claude-opus-5-5", "claude-sonnet-5-5", "claude-haiku-5-5", "claude-fable-5-1"];
      case "ai_chat":
        stream(a.onEvent, aiReply(a.messages as ChatMessage[], Boolean(a.agent)), !a.agent);
        return nextId++;

      case "sftp_open":
      case "ftp_open":
        later(300, () => send(a.onEvent, { type: "status", stage: "connected" }));
        return nextId++;
      case "sftp_realpath":
      case "ftp_realpath":
        return "/root";
      case "sftp_list":
      case "ftp_list":
        return remoteListing(String(a.path));
      case "sftp_download":
        return transfer(a.onProgress, String(a.remote).split("/").pop()!, 2_210_000);
      case "sftp_upload":
        return transfer(a.onProgress, String((a.locals as string[])[0]).split("/").pop()!, 4_800_000);
      case "sftp_read_text":
      case "ftp_read_text":
      case "local_read_text":
        return demoText(String(a.path));
      case "sftp_write_text":
      case "ftp_write_text":
      case "local_write_text":
        return Math.floor(Date.now() / 1000);
      case "local_home":
        return "/Users/demo";
      case "local_list": {
        const path = String(a.path);
        const now = Math.floor(Date.now() / 1000);
        const entry = (name: string, isDir: boolean, size: number, age: number) => ({
          name,
          path: `${path}/${name}`,
          isDir,
          isSymlink: false,
          size,
          modified: now - age,
          permissions: isDir ? 0o755 : 0o644,
          owner: null,
        });
        return {
          path,
          parent: path === "/" ? null : path.replace(/\/[^/]+$/, "") || "/",
          entries: [
            entry("Desktop", true, 0, 3600),
            entry("Documents", true, 0, 86400),
            entry("Downloads", true, 0, 600),
            entry("backup.sql.gz", false, 48_300_000, 1800),
            entry("nginx.conf", false, 2_210, 5400),
            entry("deploy.sh", false, 980, 9000),
          ],
        };
      }

      case "ssh_keys_list":
        return KEYS;
      case "known_hosts_list":
        return KNOWN;
      case "ssh_keys_default_comment":
        return "demo@macbook";

      case "hostealo_account":
        return null;
      case "hostealo_connect":
        return new Promise((resolve) => later(700, () => resolve({ name: "Tu empresa SL", email: "demo@example.com" })));
      case "hostealo_sync": {
        const added = [
          { ...host("h-vps-1", name("vps-tienda"), "203.0.113.70", "Hostealo/VPS"), hostealo: { serviceId: "1201", kind: "vps" as const } },
          { ...host("h-vps-2", name("vps-correo"), "203.0.113.71", "Hostealo/VPS"), hostealo: { serviceId: "1202", kind: "vps" as const } },
          { ...host("h-ded-1", name("dedicado-01"), "203.0.113.90", "Hostealo/Dedicados"), hostealo: { serviceId: "877", kind: "dedicated" as const } },
        ].filter((h) => !data.hosts.some((existing) => existing.id === h.id));
        data.hosts.push(...added);
        for (const folder of ["Hostealo", "Hostealo/VPS", "Hostealo/Dedicados"]) if (!data.folders.includes(folder)) data.folders.push(folder);
        return new Promise((resolve) => later(900, () => resolve({ added: added.length, updated: 0, unchanged: 0, pending: 0, data: hostsData() })));
      }
      case "hostealo_power":
        return new Promise((resolve) => later(800, () => resolve("ok")));

      case "import_detect":
        return [
          { format: "sshConfig", path: "/Users/demo/.ssh/config", count: 3 },
          { format: "fileZilla", path: "/Users/demo/.config/filezilla/sitemanager.xml", count: 2 },
        ];
      case "import_read":
        return {
          token: 1,
          format: "mobaXterm",
          source: "/Users/demo/Downloads/MobaXterm Sessions.mxtsessions",
          hosts: IMPORTABLE,
          warnings: [],
        };
      case "import_apply": {
        const into = (a.into as string | null) ?? null;
        for (const index of a.selected as number[]) {
          const item = IMPORTABLE[index];
          const folder = [into, item.folder].filter(Boolean).join("/") || null;
          data.hosts.push({ ...host(`imp-${nextId++}`, item.name, item.hostname, folder, null, item.protocol), port: item.port, username: item.username });
          if (folder && !data.folders.includes(folder)) data.folders.push(folder);
        }
        return hostsData();
      }

      case "sync_listen":
        later(150, () =>
          send(a.onEvent, { type: "listening", device: isIOS ? "iPhone" : "MacBook-Pro", addresses: ["192.168.1.34:53211"], pairing: "hostealo-terminal://sync?a=192.168.1.34:53211&t=q7Zk2cP9vXn4LmR8sT1wYg" }),
        );
        return nextId++;
      case "sync_browse":
        // On a phone the other device is the computer, and the other way round.
        later(1400, () => send(a.onEvent, isIOS ? { type: "found", id: "mac", name: "MacBook-Pro", address: "192.168.1.52:60412" } : { type: "found", id: "iphone", name: "iPhone", address: "192.168.1.52:60412" }));
        return nextId++;
      case "sync_connect": {
        syncEvents = a.onEvent;
        const verified = String(a.address).startsWith("hostealo-terminal://");
        later(600, () => send(a.onEvent, { type: "code", peer: verified ? "PC-Oficina" : "iPhone", code: "482 913", verified }));
        return nextId++;
      }
      case "sync_confirm":
        later(500, () => send(syncEvents, { type: "syncing" }));
        later(1500, () => send(syncEvents, { type: "done", summary: { added: 2, updated: 1, deleted: 0, secrets: a.includeSecrets ? 3 : 0 } }));
        return null;
      case "sync_export":
        return new Promise((resolve) => later(900, () => resolve({ hosts: data.hosts.length, secrets: a.includeSecrets ? 4 : 0 })));
      case "sync_import":
        return new Promise((resolve) => later(900, () => resolve({ added: 0, updated: 0, deleted: 0, secrets: 0 })));
      case "sync_qr_export": {
        // The size of a real transfer: one dense code, or three with the passwords.
        const part = (i: number, n: number) => `hostealo-terminal://qr?s=d3m0&i=${i}&n=${n}&k=${a.includeSecrets ? "" : "K7QM2XPA"}&d=${"Q".repeat(620 + i * 7)}`;
        const n = a.includeSecrets ? 3 : 1;
        return new Promise((resolve) =>
          later(400, () => resolve({ frames: Array.from({ length: n }, (_, i) => part(i + 1, n)), code: a.includeSecrets ? "K7QM2XPA" : null, hosts: data.hosts.length, secrets: a.includeSecrets ? 4 : 0 })),
        );
      }
      case "sync_qr_import":
        return new Promise((resolve) => later(700, () => resolve({ added: 6, updated: 0, deleted: 0, secrets: a.code ? 4 : 0 })));
      // ?update: a new version is out (the sidebar notice, then the download).
      case "plugin:updater|check":
        return params.has("update") ? { rid: 7, currentVersion: "0.1.2", version: "0.1.3", date: null, body: "", rawJson: {} } : null;
      case "plugin:updater|download_and_install": {
        const total = 18_400_000;
        send(a.onEvent, { event: "Started", data: { contentLength: total } });
        for (let step = 1; step <= 10; step++) later(step * 250, () => send(a.onEvent, { event: "Progress", data: { chunkLength: total / 10 } }));
        return new Promise((resolve) => later(2700, () => resolve(null)));
      }
      case "plugin:process|restart":
        return new Promise(() => {});
      case "plugin:dialog|save":
        return "/Users/demo/Documents/hostealo-terminal.hostealo";
      case "plugin:dialog|open":
        return JSON.stringify(a).includes("hostealo") ? "/Users/demo/Downloads/hostealo-terminal.hostealo" : ["/Users/demo/Downloads/MobaXterm Sessions.mxtsessions"];

      default:
        // Resizes, closes, opener, notifications...: nothing to do.
        return null;
    }
  },
  { shouldMockEvents: true },
);

// Preferences: the visitor's theme, unless the page asks for one.
const theme = params.get("theme") ?? (matchMedia("(prefers-color-scheme: light)").matches ? "light" : "dark");
try {
  localStorage.setItem("hostealo-terminal:settings", JSON.stringify({ theme, language, collapsedFolders: [folderName("Clientes/Beta")] }));
} catch {
  // Storage blocked: defaults apply.
}

// Opened on its own (not inside the website), say it is a demo.
// ?clean: no badge (recordings for the README).
if (window.top === window && !params.has("clean")) {
  const badge = document.body.appendChild(document.createElement("a"));
  badge.href = "https://hostealo.es/terminal";
  badge.textContent = language === "es" ? "Demo · sin conexiones reales · Descargar la app →" : "Demo · no real connections · Get the app →";
  badge.style.cssText =
    "position:fixed;left:50%;bottom:44px;transform:translateX(-50%);z-index:100;padding:6px 14px;border-radius:999px;background:#4f46e5;color:#fff;font:500 12px/1.4 system-ui,sans-serif;text-decoration:none;box-shadow:0 6px 20px rgba(0,0,0,.25)";
}

await import("../main");
const { useTabs } = await import("../store/tabs");
const { useHosts } = await import("../store/hosts");
await useHosts.getState().load();
// ?files: start on the file browser instead of a terminal.
if (params.has("files")) useTabs.getState().openSftp(params.get("host") ?? "web");
else useTabs.getState().openSsh(params.get("host") ?? "web");
