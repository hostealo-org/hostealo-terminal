import { usePrompts } from "../store/prompts";
import { remoteClose, remoteExec, remoteOpen, type ExecOutput } from "./ipc";

/**
 * The SSH connection the AI agent runs commands through on a remote desktop tab: the host's
 * linked SSH host (the same Windows server with OpenSSH). Opened on the first command, kept while
 * the tab is open, and asked for host keys and passwords through the usual dialogs.
 */
interface Connection {
  hostId: string;
  ready: Promise<number>;
  id: number | null;
}

const connections = new Map<string, Connection>();

/** Prompts of these connections get their own key, so they never cancel the desktop's own. */
const promptKey = (tabId: string) => `${tabId}:commands`;

function open(tabId: string, hostId: string): Connection {
  let resolve!: (id: number) => void;
  let reject!: (error: Error) => void;
  const connection: Connection = {
    hostId,
    id: null,
    ready: new Promise<number>((res, rej) => {
      resolve = res;
      reject = rej;
    }),
  };
  // Nobody may be waiting yet when it fails; the next exec sees the error anyway.
  connection.ready.catch(() => {});
  let connected = false;
  const forget = () => {
    if (connections.get(tabId) === connection) connections.delete(tabId);
  };

  remoteOpen(hostId, (event) => {
    switch (event.type) {
      case "status":
        if (event.stage === "connected") {
          connected = true;
          if (connection.id !== null) resolve(connection.id);
        }
        return;
      case "hostKey":
      case "certificate":
      case "prompt":
        usePrompts.getState().enqueue({ ...event, tabId: promptKey(tabId), hostId });
        return;
      case "closed":
        usePrompts.getState().dropForTab(promptKey(tabId));
        forget();
        reject(new Error(event.error ?? "cancelled"));
        return;
    }
  })
    .then((id) => {
      connection.id = id;
      if (connections.get(tabId) !== connection) void remoteClose(id);
      else if (connected) resolve(id);
    })
    .catch((e) => {
      forget();
      reject(new Error(String(e)));
    });

  connections.set(tabId, connection);
  return connection;
}

/** Runs `command` on the SSH host linked to the remote desktop in `tabId`. */
export async function rdpCommandsExec(tabId: string, hostId: string, command: string, timeoutSecs: number): Promise<ExecOutput> {
  for (let attempt = 0; ; attempt++) {
    let connection = connections.get(tabId);
    if (connection && connection.hostId !== hostId) {
      closeRdpCommands(tabId);
      connection = undefined;
    }
    connection ??= open(tabId, hostId);
    const id = await connection.ready;
    try {
      return await remoteExec(id, command, timeoutSecs);
    } catch (e) {
      // The connection died since the last command: connect again, once.
      if (attempt > 0 || !String(e).includes("not connected")) throw e;
      closeRdpCommands(tabId);
    }
  }
}

export function closeRdpCommands(tabId: string) {
  const connection = connections.get(tabId);
  if (!connection) return;
  connections.delete(tabId);
  usePrompts.getState().dropForTab(promptKey(tabId));
  if (connection.id !== null) void remoteClose(connection.id);
}
