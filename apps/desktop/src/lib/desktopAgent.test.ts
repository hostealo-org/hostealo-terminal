import { describe, expect, it } from "vitest";

import { toChatMessages, type AiMessage } from "../store/ai";
import { actionBlockIn, describeAction, isPassiveAction, isRiskyAction, parseAction } from "./desktopAgent";

describe("desktop agent", () => {
  it("finds the last complete action block", () => {
    const reply = 'Abro el menú:\n```action\n{"action":"key","keys":"win"}\n```\nluego\n```action\n{"action":"click","x":10,"y":20}\n```';
    expect(actionBlockIn(reply)?.raw).toBe('{"action":"click","x":10,"y":20}');
    expect(actionBlockIn('```action\n{"action":"cli')).toBeNull();
    expect(actionBlockIn("nothing to do")).toBeNull();
  });

  it("parses valid actions and rejects the rest", () => {
    expect(parseAction('{"action":"click","x":5,"y":6,"clicks":2}')).toEqual({ action: "click", x: 5, y: 6, button: "left", clicks: 2 });
    expect(parseAction('{"action":"key","keys":"ctrl+shift+esc"}')).toEqual({ action: "key", keys: "ctrl+shift+esc" });
    expect(parseAction('{"action":"wait","seconds":60}')).toEqual({ action: "wait", seconds: 10 });
    expect(parseAction('{"action":"key","keys":"ctrl+banana"}')).toBeNull();
    expect(parseAction('{"action":"click","x":"5","y":6}')).toBeNull();
    expect(parseAction('{"action":"type","text":""}')).toBeNull();
    expect(parseAction("not json")).toBeNull();
    expect(parseAction('{"action":"format_disk"}')).toBeNull();
  });

  it("describes actions and flags the risky ones", () => {
    expect(describeAction({ action: "click", x: 1, y: 2, button: "right" })).toBe("right-click (1, 2)");
    expect(isRiskyAction({ action: "type", text: "Remove-Item C:\\data -Recurse -Force" })).toBe(true);
    expect(isRiskyAction({ action: "type", text: "services.msc" })).toBe(false);
    expect(isRiskyAction({ action: "key", keys: "alt+F4" })).toBe(true);
    expect(isPassiveAction({ action: "screenshot" })).toBe(true);
    expect(isPassiveAction({ action: "click", x: 0, y: 0 })).toBe(false);
  });

  it("resends only the newest screenshots", () => {
    const image = (n: number) => ({ mediaType: "image/jpeg" as const, data: `img${n}` });
    const messages: AiMessage[] = [
      { id: "1", role: "user", text: "q", images: [image(1)] },
      { id: "2", role: "assistant", text: "a", state: "done" },
      { id: "3", role: "user", hidden: true, text: "r1", images: [image(2)] },
      { id: "4", role: "assistant", text: "b", state: "done" },
      { id: "5", role: "user", hidden: true, text: "r2", images: [image(3)] },
    ];
    const chat = toChatMessages(messages);
    expect(chat[0].images).toBeUndefined();
    expect(chat[0].content).toContain("older screenshot");
    expect(chat[2].images?.[0].data).toBe("img2");
    expect(chat[4].images?.[0].data).toBe("img3");
  });
});
