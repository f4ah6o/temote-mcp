import { App, applyDocumentTheme, applyHostStyleVariables } from "@modelcontextprotocol/ext-apps";
import { OpenAIExtensions } from "@openai/mcp-extensions/app";
import { FabricController, refreshDelay } from "./controller.js";
import "@openai/mcp-extensions/app/styles.css";

const app = new App({ name: "Temote Fabric", version: "1" }, { availableDisplayModes: ["inline", "fullscreen"] });
new OpenAIExtensions(app);
const $ = (id) => document.getElementById(id);
let connected = false;
let suspended = false;
let timer;
const controller = new FabricController((params) => app.callServerTool(params), render);

function text(tag, value, className = "") {
  const node = document.createElement(tag);
  node.textContent = String(value ?? "Unknown");
  if (className) node.className = className;
  return node;
}

function list(container, entries, selection, selected) {
  container.replaceChildren();
  if (!entries.length) return container.append(text("p", "No items available."));
  const wrapper = document.createElement("div");
  wrapper.className = "list";
  for (const entry of entries) {
    const button = document.createElement("button");
    button.type = "button";
    button.className = "row cursor-interaction";
    button.setAttribute("aria-pressed", String(entry.id === selected));
    button.append(text("span", entry.id, "label"), text("span", entry.status, "state"), text("span", entry.meta, "meta"));
    button.addEventListener("click", () => { void selection(entry.id); });
    wrapper.append(button);
  }
  container.append(wrapper);
}

function envelopeNote(envelope, fallback) {
  if (!envelope) return fallback;
  if (envelope.retained || envelope.freshness === "stale") return "Saved state · stale";
  if (envelope.status === "unavailable") return `Unavailable · ${envelope.error_code ?? "unknown"}`;
  return "Current read";
}

function render(state) {
  // Preserve keyboard focus across automatic refreshes that rebuild the lists.
  const focus = document.activeElement?.dataset?.selection;
  $("refresh").disabled = !connected || state.busy;
  $("connection").textContent = state.busy ? "Refreshing…" : connected ? "Connected · read-only" : "Connecting to MCP host…";
  $("error").hidden = !state.error;
  $("error").textContent = state.error;
  const inventory = state.overview?.inventory;
  const hostRows = inventory?.data?.hosts ?? [];
  list($("hosts"), hostRows.map((host) => ({
    id: host.host_id,
    status: inventory?.retained ? `${host.availability} · stale` : host.availability,
    meta: `Replica: ${host.replica?.freshness ?? "unknown"} · ${host.replica?.last_synced_at ?? "never synchronized"}`,
  })), (id) => controller.select(id), state.hostId);
  for (const button of $("hosts").querySelectorAll("button")) button.dataset.selection = `host:${button.firstChild.textContent}`;
  $("hosts").append(text("p", envelopeNote(inventory, "Waiting for host inventory.")));

  const host = hostRows.find((candidate) => candidate.host_id === state.hostId);
  const staleHost = inventory?.retained || !host || host.availability !== "online";
  if (!state.hostId) $("sessions").replaceChildren(text("p", "Select a host to view sessions."));
  else {
    const sessions = state.sessions?.sessions;
    list($("sessions"), (sessions?.data?.sessions ?? []).map((session) => ({
      id: session.session_id, status: session.status,
      meta: staleHost || sessions.retained ? "Saved state · stale" : session.permission_mode ?? "Unknown mode",
    })), (id) => controller.select(state.hostId, id), state.sessionId);
    for (const button of $("sessions").querySelectorAll("button")) button.dataset.selection = `session:${button.firstChild.textContent}`;
    $("sessions").append(text("p", envelopeNote(sessions, "Loading sessions…")));
    if (sessions?.data?.truncated) $("sessions").append(text("p", "Session list is truncated."));
  }

  const sessionEnvelope = state.detail?.session;
  const session = sessionEnvelope?.data?.session;
  $("session-detail").textContent = session
    ? `${session.session_id} · ${session.status} · ${session.workspace?.repository ?? "Unknown repository"} · ${session.workspace?.branch ?? "Unknown branch"}`
    : state.sessionId ? envelopeNote(sessionEnvelope, "Loading session…") : "Select a session to view retained tasks.";
  $("tasks").replaceChildren();
  if (state.sessionId && state.detail) {
    const taskEnvelope = state.detail.tasks;
    $("tasks").append(text("p", staleHost ? "Saved state · host availability unconfirmed" : envelopeNote(taskEnvelope, "Tasks unavailable.")));
    for (const backend of taskEnvelope?.data?.backends ?? []) {
      $("tasks").append(text("h3", `${backend.backend} · ${backend.status}`, "backend"));
      if (backend.status !== "confirmed") {
        $("tasks").append(text("p", "Backend state unavailable."));
        continue;
      }
      const tasks = backend.tasks ?? [];
      if (!tasks.length) $("tasks").append(text("p", "No retained tasks."));
      else {
        const ul = document.createElement("ul");
        ul.className = "task-list";
        for (const task of tasks) ul.append(text("li", `${task.task_id} · ${task.status} · Saved ${task.last_updated_at ?? "at an unknown time"}`));
        $("tasks").append(ul);
      }
      if (backend.truncated) $("tasks").append(text("p", "Task list is truncated."));
    }
  }
  $("updated").textContent = state.overview?.generated_at ? `Last overview read: ${state.overview.generated_at}` : "";
  if (focus) {
    const button = [...document.querySelectorAll("button[data-selection]")].find((candidate) => candidate.dataset.selection === focus);
    button?.focus({ preventScroll: true });
  }
}

function applyContext(context) {
  if (context?.theme) applyDocumentTheme(context.theme);
  if (context?.styles?.variables) applyHostStyleVariables(context.styles.variables);
}

function schedule() {
  clearTimeout(timer);
  if (!connected || suspended) return;
  timer = setTimeout(async () => {
    await controller.refresh();
    schedule();
  }, refreshDelay(document.hidden));
}

app.ontoolresult = (result) => { controller.initial(result); };
app.addEventListener("hostcontextchanged", applyContext);
$("refresh").addEventListener("click", async () => { await controller.refresh(); schedule(); });
document.addEventListener("visibilitychange", schedule);
window.addEventListener("pagehide", () => { suspended = true; clearTimeout(timer); });
window.addEventListener("pageshow", () => { suspended = false; schedule(); });

try {
  await app.connect();
  connected = true;
  applyContext(app.getHostContext());
  render(controller.state);
  // The host delivers the initial tool result after initialization. Do not
  // duplicate fabric_overview here; the refresh timer handles later reads.
  schedule();
} catch {
  $("connection").textContent = "MCP host connection unavailable.";
  $("error").hidden = false;
  $("error").textContent = "Reopen the app to retry the connection.";
}
