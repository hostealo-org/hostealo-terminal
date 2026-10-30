// Data and helpers shared by the UI preview (preview.ts) and the website demo (demo.ts): the
// real app on a mocked backend.
import type { Channel } from "@tauri-apps/api/core";

import type { Host, KnownHostEntry, ResourceSnapshot, SftpEntry, Snippet, SshKeyInfo } from "../lib/ipc";

export const host = (
  id: string,
  name: string,
  hostname: string,
  folder: string | null,
  color: string | null = null,
  protocol: Host["protocol"] = "ssh",
): Host => ({
  id,
  name,
  hostname,
  port: protocol === "ssh" ? 22 : 21,
  username: protocol === "ssh" ? "root" : "acme",
  protocol,
  auth: { type: "password" },
  color,
  folder,
  rdpSharedFolder: protocol === "rdp" ? "~/Downloads" : null,
  hasPassword: true,
  hasPassphrase: false,
});

export const HOSTS: Host[] = [
  host("web", "web-01", "203.0.113.10", "Producción", "red"),
  host("db", "db-01", "203.0.113.11", "Producción", "red"),
  host("acme-web", "acme-web", "198.51.100.20", "Clientes/Acme", "green"),
  host("acme-mail", "acme-mail", "198.51.100.21", "Clientes/Acme", "green"),
  host("acme-ftp", "acme-hosting", "ftp.acme.example", "Clientes/Acme", "green", "ftps"),
  host("beta", "beta-shop", "198.51.100.40", "Clientes/Beta"),
  host("staging", "staging", "192.0.2.50", "Staging", "amber"),
  host("lab", "lab-pi", "192.168.1.30", null, "cyan"),
  host("win", "win-server", "203.0.113.30", "Producción", "red", "rdp"),
];
// Systems as SSH would have detected them.
export const SYSTEMS: Record<string, [string, string]> = {
  web: ["ubuntu", "Ubuntu 24.04.1 LTS"],
  db: ["rocky", "Rocky Linux 9.4 (Blue Onyx)"],
  "acme-web": ["almalinux", "AlmaLinux 9.4 (Seafoam Ocelot)"],
  "acme-mail": ["debian", "Debian GNU/Linux 12 (bookworm)"],
  beta: ["proxmox", "Proxmox VE 8.2.4"],
  staging: ["alpine", "Alpine Linux v3.20"],
  lab: ["raspberrypi", "Raspbian GNU/Linux 12 (bookworm)"],
};
for (const h of HOSTS) if (SYSTEMS[h.id]) h.os = { id: SYSTEMS[h.id][0], name: SYSTEMS[h.id][1] };

export const FOLDERS = ["Clientes", "Clientes/Acme", "Clientes/Beta", "Producción", "Staging"];

export const SNIPPETS: Snippet[] = [
  { id: "s-disk", name: "Espacio en disco", command: "df -h" },
  { id: "s-docker", name: "Contenedores", command: "docker ps" },
  { id: "s-logs", name: "Logs de nginx", command: "tail -n 50 /var/log/nginx/access.log" },
  { id: "s-reload", name: "Recargar nginx", command: "nginx -t && systemctl reload nginx" },
  { id: "s-update", name: "Actualizar paquetes", command: "apt update\napt install -y htop" },
  { id: "s-reboot", name: "Reiniciar el servidor", command: "reboot" },
];

/** Quick commands kept in memory by the mocks (save and delete work). */
export function snippetStore(initial: Snippet[]) {
  let all = [...initial];
  return {
    list: () => all,
    save: (snippet: Snippet) => {
      const saved = { ...snippet, id: snippet.id || crypto.randomUUID(), name: snippet.name.trim() || snippet.command.split("\n")[0] };
      all = all.some((s) => s.id === saved.id) ? all.map((s) => (s.id === saved.id ? saved : s)) : [...all, saved];
      return all;
    },
    remove: (id: string) => (all = all.filter((s) => s.id !== id)),
  };
}

export const TCPDUMP = [
  "\x1b[01;32mroot@web-01\x1b[00m:\x1b[01;34m~\x1b[00m# tcpdump -ni eth0 port 443",
  "tcpdump: verbose output suppressed, use -v[v]... for full protocol decode",
  "listening on eth0, link-type EN10MB (Ethernet), snapshot length 262144 bytes",
  "12:41:07.512399 IP 203.0.113.10.443 > 192.0.2.44.51522: Flags [P.], seq 1:518, ack 1, win 501, length 517",
  "12:41:07.513021 IP 192.0.2.44.51522 > 203.0.113.10.443: Flags [.], ack 518, win 2047, length 0",
  "12:41:07.640115 IP 192.0.2.17.40112 > 203.0.113.10.443: Flags [S], seq 2843, win 64240, length 0",
  "",
  "\x1b[01;32mroot@web-01\x1b[00m:\x1b[01;34m~\x1b[00m# systemctl status nginx --no-pager | head -3",
  "● nginx.service - A high performance web server",
  "     Loaded: loaded (/lib/systemd/system/nginx.service; enabled)",
  "     Active: active (running) since 2026-10-15 09:12:44 CEST; 3h ago",
  "",
  "\x1b[01;32mroot@web-01\x1b[00m:\x1b[01;34m~\x1b[00m# journalctl -u ssh -n 3 --no-pager",
  "Oct 15 12:39:58 web-01 sshd[20311]: Failed password for root from 192.0.2.66 port 41022 ssh2",
  "Oct 15 12:40:02 web-01 sshd[20311]: Connection closed by authenticating user root 192.0.2.66 port 41022",
  "Oct 15 12:40:31 web-01 sshd[20340]: Accepted publickey for root from 192.0.2.44 port 51522 ssh2",
  "",
  "\x1b[01;32mroot@web-01\x1b[00m:\x1b[01;34m~\x1b[00m# ",
].join("\r\n");

export const LOCAL = "\x1b[1;35m➜\x1b[0m  \x1b[36m~\x1b[0m ls\r\nDesktop   Documents   Downloads   Projects\r\n\x1b[1;35m➜\x1b[0m  \x1b[36m~\x1b[0m ";

let cpu = 20;
export function snapshot(remote: boolean): ResourceSnapshot {
  cpu = Math.max(3, Math.min(97, cpu + (Math.random() - 0.45) * 18));
  return {
    hostname: remote ? "web-01" : "macbook",
    os: remote ? "Ubuntu 24.04.1 LTS" : "macOS 26.0",
    cpuPercent: cpu,
    cpuCount: remote ? 8 : 12,
    memUsed: (remote ? 6.1 : 21.4) * 2 ** 30,
    memTotal: (remote ? 16 : 36) * 2 ** 30,
    swapUsed: 0.2 * 2 ** 30,
    swapTotal: 2 * 2 ** 30,
    disks: [
      { mount: "/", used: 61 * 2 ** 30, total: 160 * 2 ** 30 },
      { mount: "/var/lib/mysql", used: 140 * 2 ** 30, total: 200 * 2 ** 30 },
    ],
    netRxBps: 1.2e6 * Math.random() + 2e5,
    netTxBps: 3e5 * Math.random() + 4e4,
    load: [0.42, 0.37, 0.31],
    uptimeSecs: 12 * 86400 + 4 * 3600,
  };
}

/** A desktop-like picture as an RDP frame message (see src-tauri/src/rdp.rs). */
export function fakeDesktop(width: number, height: number): ArrayBuffer {
  const canvas = new OffscreenCanvas(width, height);
  const c = canvas.getContext("2d")!;
  const sky = c.createLinearGradient(0, 0, width, height);
  sky.addColorStop(0, "#0b3a75");
  sky.addColorStop(1, "#1d72c4");
  c.fillStyle = sky;
  c.fillRect(0, 0, width, height);
  c.fillStyle = "#f3f3f3";
  c.fillRect(160, 110, 640, 400);
  c.fillStyle = "#ffffff";
  c.fillRect(160, 110, 640, 32);
  c.fillStyle = "#1f1f1f";
  c.font = "14px sans-serif";
  c.fillText("Server Manager", 176, 131);
  c.fillText("Welcome to Windows Server", 190, 180);
  c.fillStyle = "rgba(255,255,255,0.85)";
  c.fillRect(0, height - 48, width, 48);
  const pixels = c.getImageData(0, 0, width, height).data;
  const message = new Uint8Array(9 + pixels.length);
  const view = new DataView(message.buffer);
  [0, 0, width, height].forEach((value, i) => view.setUint16(1 + i * 2, value, true));
  message.set(pixels, 9);
  return message.buffer;
}

export const AI_REPLY = `El tráfico que ves es casi todo **HTTPS (443)** hacia \`203.0.113.10\`; las peticiones a \`:22\` son tu propia sesión SSH. No hay nada raro, pero hay muchas conexiones nuevas desde la misma IP.

Para ver qué IPs abren más conexiones ahora mismo:

\`\`\`bash
ss -Hnt state established '( sport = :443 )' | awk '{print $4}' | cut -d: -f1 | sort | uniq -c | sort -rn | head
\`\`\`

Si una IP destaca mucho, puedes bloquearla temporalmente:

\`\`\`bash
iptables -I INPUT -s 198.51.100.7 -j DROP
\`\`\`

- Antes de bloquear, comprueba que **no es tu propia IP**.
- La regla se pierde al reiniciar; para hacerla permanente usa \`iptables-save\`.`;

export const send = (channel: unknown, message: unknown) => (channel as Channel<unknown>).onmessage(message);
export const bytes = (text: string) => new TextEncoder().encode(text).buffer;
export const later = (ms: number, fn: () => void) => setTimeout(fn, ms);

export const KEYS: SshKeyInfo[] = [
  {
    name: "id_ed25519",
    path: "/Users/demo/.ssh/id_ed25519",
    keyType: "ssh-ed25519",
    fingerprint: "SHA256:Xq8m9sT3vC4yWgZ0kP2r7uN1fH5eL6dA9bJ0cQ3iK8o",
    comment: "demo@macbook",
    encrypted: true,
    publicKey: "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIOMqqnkVzrm0SdG6UOoqKLsabgH5C9okWi0dh2l9GKJl demo@macbook",
    modified: 1760000000,
  },
  {
    name: "id_rsa",
    path: "/Users/demo/.ssh/id_rsa",
    keyType: "ssh-rsa",
    fingerprint: "SHA256:4bK2pQ9xR1sT6uV8wY0zA3cE5gH7jL9nM2oP4qS6tU8",
    comment: "old-laptop",
    encrypted: false,
    publicKey: "ssh-rsa AAAAB3NzaC1yc2E... old-laptop",
    modified: 1600000000,
  },
];

export const KNOWN: KnownHostEntry[] = [
  { source: "app", line: 1, raw: "", hosts: ["203.0.113.10"], hashed: false, marker: null, keyType: "ssh-ed25519", fingerprint: "SHA256:Hq3V7l2kQe6rTn9pWx1sYb4cZa8dFg0hJu5iKm3oNp2" },
  { source: "user", line: 4, raw: "", hosts: [], hashed: true, marker: null, keyType: "ecdsa-sha2-nistp256", fingerprint: "SHA256:p0Oi9Uy8Tr7Ew6Qa5Sd4Fg3Hj2Kl1Zx0Cv9Bn8Mq7Wd" },
  { source: "user", line: 7, raw: "", hosts: ["[198.51.100.20]:2222"], hashed: false, marker: null, keyType: "ssh-ed25519", fingerprint: "SHA256:a1B2c3D4e5F6g7H8i9J0k1L2m3N4o5P6q7R8s9T0u1V" },
];

export const SFTP: SftpEntry[] = [
  { name: "etc", path: "/root/etc", isDir: true, isSymlink: false, size: 4096, modified: 1760500000, permissions: 0o755, owner: "root" },
  { name: "www", path: "/root/www", isDir: true, isSymlink: true, size: 13, modified: 1760400000, permissions: 0o777, owner: "root" },
  { name: ".env", path: "/root/.env", isDir: false, isSymlink: false, size: 214, modified: 1760490000, permissions: 0o600, owner: "root" },
  { name: ".bashrc", path: "/root/.bashrc", isDir: false, isSymlink: false, size: 3771, modified: 1750000000, permissions: 0o644, owner: "root" },
  { name: "backup-2026-10-14.tar.gz", path: "/root/backup-2026-10-14.tar.gz", isDir: false, isSymlink: false, size: 1.8 * 2 ** 30, modified: 1760480000, permissions: 0o600, owner: "root" },
  { name: "deploy.sh", path: "/root/deploy.sh", isDir: false, isSymlink: false, size: 2210, modified: 1760510000, permissions: 0o755, owner: "root" },
];


/** A server's Docker, as `docker ps` / `docker stats` print it (mocked `remote_exec`). */
const DOCKER_CONTAINERS: [string, string, string, string, string, string, string][] = [
  // id, name, image, state, status, ports, compose project
  ["a3f9c1e2b7d4", "tienda-web-1", "ghcr.io/acme/tienda:2.4.1", "running", "Up 3 hours", "0.0.0.0:3000->3000/tcp", "tienda"],
  ["b81d0c4e9a12", "tienda-db-1", "postgres:17", "running", "Up 12 days (healthy)", "5432/tcp", "tienda"],
  ["c4e2a9f1d3b8", "tienda-cache-1", "redis:7-alpine", "running", "Up 12 days", "6379/tcp", "tienda"],
  ["d7a1e3c5b9f2", "monitor-grafana-1", "grafana/grafana:11.3.0", "running", "Up 5 days", "0.0.0.0:3001->3000/tcp", "monitor"],
  ["e2b8f4a6c1d3", "monitor-prometheus-1", "prom/prometheus:v3.0.0", "running", "Up 5 days", "9090/tcp", "monitor"],
  ["f9c3d5e7a2b4", "traefik", "traefik:v3.2", "running", "Up 12 days", "0.0.0.0:80->80/tcp, 0.0.0.0:443->443/tcp", ""],
  ["0a1b2c3d4e5f", "backup-nightly", "restic/restic:0.17.3", "exited", "Exited (0) 6 hours ago", "", ""],
];

export function dockerExec(command: string, state: Map<string, string>): { stdout: string; stderr: string; exitCode: number; timedOut: boolean } {
  const ok = (stdout: string) => ({ stdout, stderr: "", exitCode: 0, timedOut: false });
  if (command.includes("version")) return ok("27.3.1\n");
  if (command.includes(" ps ")) {
    return ok(
      DOCKER_CONTAINERS.map(([id, name, image, initial, status, ports, project]) => {
        const current = state.get(id) ?? initial;
        const shown = current === initial ? status : current === "running" ? "Up 1 second" : "Exited (0) 1 second ago";
        return JSON.stringify({ ID: id.padEnd(64, "0"), Names: name, Image: image, State: current, Status: shown, Ports: current === "running" ? ports : "", Labels: project ? `com.docker.compose.project=${project}` : "" });
      }).join("\n") + "\n",
    );
  }
  if (command.includes(" stats ")) {
    return ok(
      DOCKER_CONTAINERS.filter(([id, , , initial]) => (state.get(id) ?? initial) === "running")
        .map(([id, name], i) => JSON.stringify({ ID: id, Name: name, CPUPerc: `${(Math.random() * (i === 0 ? 30 : 4)).toFixed(2)}%`, MemUsage: `${[182, 96, 12, 140, 210, 48][i] ?? 30}MiB / 7.6GiB`, MemPerc: "2.1%" }))
        .join("\n") + "\n",
    );
  }
  if (command.includes(" logs ")) {
    const now = Date.now();
    return ok(
      Array.from({ length: 40 }, (_, i) => {
        const at = new Date(now - (40 - i) * 7000).toISOString();
        return i % 9 === 4
          ? `${at} WARN  slow query took 1.2s: SELECT * FROM orders WHERE status = 'pending'`
          : `${at} INFO  GET /api/products/${100 + i} 200 ${12 + (i % 7)}ms`;
      }).join("\n"),
    );
  }
  const action = /docker (start|stop|restart|rm -f) ([a-f0-9]+)/.exec(command);
  if (action) {
    const id = action[2].slice(0, 12);
    if (action[1] === "rm -f") {
      const index = DOCKER_CONTAINERS.findIndex(([cid]) => cid === id);
      if (index >= 0) DOCKER_CONTAINERS.splice(index, 1);
    } else state.set(id, action[1] === "stop" ? "exited" : "running");
    return ok(`${id}\n`);
  }
  return ok("");
}

/** A two-node Proxmox cluster, as `pvesh get /cluster/resources --type vm` prints it. */
const PROXMOX = [
  { type: "qemu", vmid: 101, name: "web-prod", node: "pve1", status: "running", cpu: 0.18, maxcpu: 4, mem: 3.1 * 2 ** 30, maxmem: 8 * 2 ** 30, disk: 0, maxdisk: 80 * 2 ** 30, uptime: 864000 },
  { type: "qemu", vmid: 102, name: "db-prod", node: "pve1", status: "running", cpu: 0.42, maxcpu: 8, mem: 14.2 * 2 ** 30, maxmem: 16 * 2 ** 30, disk: 0, maxdisk: 200 * 2 ** 30, uptime: 864000 },
  { type: "lxc", vmid: 103, name: "dns", node: "pve1", status: "running", cpu: 0.01, maxcpu: 1, mem: 61 * 2 ** 20, maxmem: 512 * 2 ** 20, disk: 1.2 * 2 ** 30, maxdisk: 8 * 2 ** 30, uptime: 1728000 },
  { type: "qemu", vmid: 201, name: "staging", node: "pve2", status: "stopped", cpu: 0, maxcpu: 2, mem: 0, maxmem: 4 * 2 ** 30, disk: 0, maxdisk: 40 * 2 ** 30, uptime: 0 },
  { type: "lxc", vmid: 202, name: "monitoring", node: "pve2", status: "running", cpu: 0.07, maxcpu: 2, mem: 900 * 2 ** 20, maxmem: 2 * 2 ** 30, disk: 6.4 * 2 ** 30, maxdisk: 16 * 2 ** 30, uptime: 432000 },
  { type: "qemu", vmid: 9000, name: "ubuntu-24.04-plantilla", node: "pve1", status: "stopped", maxcpu: 2, maxmem: 2 * 2 ** 30, maxdisk: 20 * 2 ** 30, template: 1 },
];

/** The mocked hypervisor (Proxmox) for `remote_exec`, or null when the command is not for it. */
export function proxmoxExec(command: string): { stdout: string; stderr: string; exitCode: number; timedOut: boolean } | null {
  const ok = (stdout: string) => ({ stdout, stderr: "", exitCode: 0, timedOut: false });
  if (command.includes("command -v pvesh")) return ok("proxmox\npve1\n");
  if (command.includes("get /version")) return ok('{"release":"8.2","repoid":"faa83925c9641325","version":"8.2.4"}\n');
  if (command.includes("/cluster/resources")) {
    return ok(JSON.stringify(PROXMOX.map((vm) => (vm.status === "running" ? { ...vm, cpu: Math.max(0.005, (vm.cpu ?? 0) + (Math.random() - 0.5) * 0.06) } : vm))));
  }
  const action = /\/nodes\/[\w-]+\/(qemu|lxc)\/(\d+)\/status\/(\w+)/.exec(command);
  if (action) {
    const vm = PROXMOX.find((v) => v.vmid === Number(action[2]));
    if (vm) {
      vm.status = action[3] === "start" || action[3] === "reboot" ? "running" : "stopped";
      vm.uptime = vm.status === "running" ? (action[3] === "start" ? 1 : vm.uptime) : 0;
      if (vm.status === "running" && !vm.mem) vm.mem = vm.maxmem * 0.3;
    }
    return ok(`UPID:pve1:0001:${action[3]}\n`);
  }
  return null;
}
