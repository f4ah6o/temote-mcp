import { createServer } from "node:http";
import appHtml from "../src/fabric-app/generated.js";

function hostHarness() {
  const harness = window.fabricHarness = { calls: [], offline: false, failed: false, partial: false, delay: 0, dark: false, frame: null };
  const envelope = (data, status = "confirmed") => ({ status, freshness: status === "confirmed" ? "live" : "stale", data });
  const overview = () => ({ kind: "fabric_overview", generated_at: "2026-10-01T01:00:00Z",
    service: envelope({ service: "temote-fabric" }),
    inventory: envelope({ hosts: [
      { host_id: "host-a", availability: harness.offline ? "offline" : "online", replica: { freshness: "stale", last_synced_at: "2026-10-01T00:59:00Z" } },
      { host_id: "host-b", availability: "unknown", replica: { freshness: "unknown" } },
      { host_id: "host-with-a-very-long-identifier-that-must-wrap-without-clipping", availability: "online", replica: { freshness: "stale" } },
    ] }),
  });
  const send = (target, payload) => target.postMessage({ jsonrpc: "2.0", ...payload }, "*");
  harness.theme = (dark) => {
    harness.dark = dark;
    send(harness.frame ?? window, { method: "ui/notifications/host-context-changed", params: { theme: dark ? "dark" : "light" } });
  };
  window.addEventListener("message", (event) => {
    const message = event.data;
    if (!message || message.jsonrpc !== "2.0") return;
    const target = event.source;
    // In the standalone gate fixture parent === self. Consume outbound
    // requests before the SDK mistakes them for inbound server requests.
    if (message.method && !["ui/notifications/tool-input", "ui/notifications/tool-result", "ui/notifications/host-context-changed"].includes(message.method)) event.stopImmediatePropagation();
    if (message.method === "ui/initialize") {
      harness.frame = target;
      send(target, { id: message.id, result: {
        protocolVersion: "2026-01-26", hostInfo: { name: "fabric-test-host", version: "1" },
        hostCapabilities: { serverTools: {}, serverResources: {} },
        hostContext: { theme: "light", displayMode: "fullscreen", availableDisplayModes: ["inline", "fullscreen"] },
      } });
    } else if (message.method === "ui/notifications/initialized") {
      send(target, { method: "ui/notifications/tool-input", params: { arguments: {} } });
      send(target, { method: "ui/notifications/tool-result", params: { content: [], structuredContent: overview() } });
    } else if (message.method === "tools/call") {
      const { name, arguments: args } = message.params;
      harness.calls.push({ name, args });
      if (harness.failed) return send(target, { id: message.id, error: { code: -32603, message: "test-failure" } });
      let view;
      if (name === "fabric_overview") view = overview();
      else if (name === "fabric_session_list") view = { kind: name, sessions: envelope({ host_id: args.host_id, sessions: [
        { session_id: "session-one", status: "active", permission_mode: "agent" },
        { session_id: "session-two", status: "active", permission_mode: "ask" },
      ] }) };
      else view = { kind: name,
        session: envelope({ ...args, session: { session_id: args.session_id, status: "active", workspace: { repository: "temote-mcp", branch: "main" } } }),
        tasks: envelope({ ...args, backends: [
          { backend: "codex", status: "confirmed", tasks: [{ task_id: `${args.session_id}-task`, status: "running", last_updated_at: "2026-10-01T01:00:00Z" }] },
          { backend: "opencode", status: harness.partial ? "unavailable" : "confirmed", tasks: [] },
        ] }),
      };
      setTimeout(() => send(target, { id: message.id, result: { content: [], structuredContent: view } }), harness.delay);
    } else if (message.id !== undefined && message.method?.startsWith("ui/")) {
      send(target, { id: message.id, result: {} });
    }
  });
}

export async function startFabricFixture(port = 0) {
  const harnessScript = `<script>(${hostHarness.toString()})();</script>`;
  const rendered = appHtml.replace('<script type="module">', `${harnessScript}<script type="module">`);
  const host = `<!doctype html><html lang="en"><head><title>Fabric MCP test host</title></head><body style="margin:0">${harnessScript}<iframe title="Temote Fabric App" src="/app" style="border:0;width:100%;height:950px" sandbox="allow-scripts allow-same-origin"></iframe></body></html>`;
  const server = createServer((request, response) => {
    response.setHeader("content-type", "text/html; charset=utf-8");
    if (request.url !== "/host") response.setHeader("content-security-policy", "default-src 'none'; script-src 'self' 'unsafe-inline'; style-src 'self' 'unsafe-inline'; img-src data:; connect-src 'none'; frame-src 'none'; base-uri 'none'; object-src 'none'");
    response.end(request.url === "/app" ? appHtml : request.url === "/host" ? host : rendered);
  });
  await new Promise((resolve) => server.listen(port, "127.0.0.1", resolve));
  return { server, url: `http://127.0.0.1:${server.address().port}`, close: () => new Promise((resolve) => server.close(resolve)) };
}

if (process.argv[1]?.endsWith("fabric-app-fixture.mjs") && !process.env.NODE_TEST_CONTEXT) {
  const fixture = await startFabricFixture(Number(process.env.FABRIC_FIXTURE_PORT ?? 0));
  console.log(fixture.url);
}
