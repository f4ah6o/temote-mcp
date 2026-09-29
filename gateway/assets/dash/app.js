const REFRESH_FOREGROUND_MS = 5_000;
const REFRESH_BACKGROUND_MS = 30_000;
const REQUEST_TIMEOUT_MS = 10_000;
const MAX_ID_LENGTH = 128;
const MAX_TEXT_LENGTH = 1_024;
const MAX_HOSTS = 256;
const MAX_SESSIONS = 256;
const MAX_TASKS = 512;
const MAX_TIMELINE_EVENTS = 100;
const PENDING_TYPES = new Set(["permission", "question", "approval"]);
const ENVELOPE_STATUSES = new Set(["confirmed", "stale", "unavailable"]);
const PENDING_STATES = new Set(["none", "pending", "unknown", "unsupported", "unavailable"]);

function boundedText(value, maxLength = MAX_TEXT_LENGTH) {
  if (typeof value !== "string") return "";
  return value.slice(0, maxLength);
}

function shortIdentifier(value, maxLength = MAX_ID_LENGTH) {
  return boundedText(value, maxLength);
}

function setText(element, value, fallback = "—", fullValue = value) {
  const text = value == null || value === "" ? fallback : String(value);
  element.textContent = text;
  const title = typeof fullValue === "string" ? boundedText(fullValue, MAX_TEXT_LENGTH) : text;
  element.title = title.length > 18 ? title : "";
}

function parseSelection(locationLike = { search: "", hash: "" }) {
  const query = new URLSearchParams(locationLike.search ?? "");
  const rawHash = String(locationLike.hash ?? "");
  const hash = new URLSearchParams(rawHash.startsWith("#") ? rawHash.slice(1) : rawHash);
  return {
    hostId: shortIdentifier(hash.get("host") ?? query.get("host")),
    sessionId: shortIdentifier(hash.get("session") ?? query.get("session")),
  };
}

function parseTimestampMs(value) {
  if (typeof value === "number" && Number.isFinite(value)) {
    return value < 1_000_000_000_000 ? value * 1_000 : value;
  }
  if (typeof value !== "string" || value.length > 80) return NaN;
  if (/^\d{1,12}$/.test(value)) {
    const numeric = Number(value);
    return numeric < 1_000_000_000_000 ? numeric * 1_000 : numeric;
  }
  const parsed = Date.parse(value);
  return Number.isFinite(parsed) ? parsed : NaN;
}

function formatTime(value) {
  const timestamp = parseTimestampMs(value);
  if (!Number.isFinite(timestamp)) return "Unknown";
  return new Intl.DateTimeFormat(undefined, {
    dateStyle: "medium",
    timeStyle: "short",
  }).format(new Date(timestamp));
}

function compareSequence(left, right) {
  const leftNumber = typeof left === "number" ? left : Number(left);
  const rightNumber = typeof right === "number" ? right : Number(right);
  if (Number.isFinite(leftNumber) && Number.isFinite(rightNumber)) {
    return Math.sign(leftNumber - rightNumber);
  }
  const a = String(left ?? "");
  const b = String(right ?? "");
  return a === b ? 0 : a < b ? -1 : 1;
}

function samePendingContent(left, right) {
  const leftTypes = Array.isArray(left?.types) ? left.types.filter((type) => PENDING_TYPES.has(type)) : [];
  const rightTypes = Array.isArray(right?.types) ? right.types.filter((type) => PENDING_TYPES.has(type)) : [];
  return left?.state === right?.state
    && left?.count === right?.count
    && left?.truncated === right?.truncated
    && JSON.stringify(leftTypes) === JSON.stringify(rightTypes);
}

function mergePendingSummary(previous, incoming) {
  if (!previous) return incoming;
  if (!incoming) return previous;
  if (incoming.state === "unsupported") return incoming;

  const previousEpoch = previous.producer_epoch;
  const incomingEpoch = incoming.producer_epoch;
  if (previousEpoch != null && incomingEpoch != null) {
    const epochOrder = compareSequence(incomingEpoch, previousEpoch);
    if (epochOrder < 0) return previous;
    if (epochOrder > 0) return incoming;
  }

  const previousRevision = previous.summary_revision;
  const incomingRevision = incoming.summary_revision;
  if (previousRevision != null && incomingRevision != null) {
    const revisionOrder = compareSequence(incomingRevision, previousRevision);
    if (revisionOrder < 0) return previous;
    if (revisionOrder > 0) return incoming;
  }

  if (!samePendingContent(previous, incoming)) return previous;
  const previousObserved = parseTimestampMs(previous.observed_at);
  const incomingObserved = parseTimestampMs(incoming.observed_at);
  return Number.isFinite(incomingObserved) && incomingObserved >= previousObserved
    ? incoming
    : previous;
}

function mergeTaskProjection(previous, incoming) {
  if (!previous) {
    return { ...incoming, pending_interaction: incoming.pending_interaction ?? { state: "unsupported" } };
  }
  const taskOrder = compareSequence(incoming.revision, previous.revision);
  const task = taskOrder < 0 ? previous : incoming;
  const summary = mergePendingSummary(
    previous.pending_interaction ?? { state: "unsupported" },
    incoming.pending_interaction ?? { state: "unsupported" },
  );
  return { ...task, pending_interaction: summary };
}

function pendingForDisplay(summary, nowMs = Date.now()) {
  const value = summary && typeof summary === "object" ? summary : { state: "unsupported" };
  const state = PENDING_STATES.has(value.state) ? value.state : "unknown";
  if (state === "unsupported") return { ...value, display_state: state };
  const expiresAt = parseTimestampMs(value.expires_at);
  if (!Number.isFinite(expiresAt) || expiresAt <= nowMs) {
    return { ...value, display_state: "unavailable", expired: true };
  }
  return { ...value, display_state: state, expired: false };
}

class SelectionFence {
  constructor(hostId = "", sessionId = "") {
    this.value = { hostId: shortIdentifier(hostId), sessionId: shortIdentifier(sessionId) };
    this.version = 0;
  }

  select(hostId, sessionId) {
    const next = { hostId: shortIdentifier(hostId), sessionId: shortIdentifier(sessionId) };
    if (next.hostId === this.value.hostId && next.sessionId === this.value.sessionId) {
      return this.snapshot();
    }
    this.value = next;
    this.version += 1;
    return this.snapshot();
  }

  snapshot() {
    return { ...this.value, version: this.version };
  }

  matches(snapshot) {
    return Boolean(snapshot)
      && snapshot.version === this.version
      && snapshot.hostId === this.value.hostId
      && snapshot.sessionId === this.value.sessionId;
  }
}

class RequestCoordinator {
  constructor(fetchFunction = (...args) => fetch(...args)) {
    this.fetchFunction = fetchFunction;
    this.inFlight = new Map();
  }

  abortSelectionExcept(version) {
    for (const entry of this.inFlight.values()) {
      if (entry.selectionVersion != null && entry.selectionVersion !== version) {
        entry.controller.abort();
      }
    }
  }

  async request(path, key, selectionVersion = null) {
    const prior = this.inFlight.get(key);
    if (prior && prior.selectionVersion === selectionVersion && !prior.controller.signal.aborted) {
      return prior.promise;
    }
    prior?.controller.abort();

    const controller = new AbortController();
    let timedOut = false;
    const timer = setTimeout(() => {
      timedOut = true;
      controller.abort();
    }, REQUEST_TIMEOUT_MS);
    const entry = { controller, selectionVersion, promise: null };
    entry.promise = (async () => {
      try {
        const response = await this.fetchFunction(path, {
          method: "GET",
          credentials: "same-origin",
          cache: "no-store",
          headers: { accept: "application/json" },
          signal: controller.signal,
        });
        let body;
        try {
          body = await response.json();
        } catch {
          body = null;
        }
        if (!response.ok) {
          return unavailableEnvelope(response.status === 401 || response.status === 403
            ? "access_denied"
            : `http_${response.status}`);
        }
        if (!body || typeof body !== "object" || !ENVELOPE_STATUSES.has(body.status)) {
          return unavailableEnvelope("invalid_response");
        }
        return body;
      } catch (error) {
        if (controller.signal.aborted) {
          return unavailableEnvelope(timedOut ? "request_timeout" : "request_cancelled");
        }
        return unavailableEnvelope(error?.name === "TypeError" ? "network_unavailable" : "request_failed");
      } finally {
        clearTimeout(timer);
        if (this.inFlight.get(key) === entry) this.inFlight.delete(key);
      }
    })();
    this.inFlight.set(key, entry);
    return entry.promise;
  }
}

function unavailableEnvelope(errorCode) {
  return {
    status: "unavailable",
    authority: "unavailable",
    freshness: "unavailable",
    error_code: boundedText(String(errorCode ?? "unavailable"), 64),
  };
}

function createElement(documentRef, tagName, className = "", text = undefined) {
  const element = documentRef.createElement(tagName);
  if (className) element.className = className;
  if (text !== undefined) setText(element, text);
  return element;
}

function appendText(documentRef, parent, tagName, className, text) {
  const element = createElement(documentRef, tagName, className, text);
  parent.append(element);
  return element;
}

function setComponentState(element, message, kind = "") {
  element.className = `component-state${kind ? ` component-state--${kind}` : ""}`;
  setText(element, message);
}

  function componentMessage(envelope, label) {
  if (!envelope) return `Loading ${label}…`;
  const status = ENVELOPE_STATUSES.has(envelope.status) ? envelope.status : "unavailable";
  if (status === "unavailable") {
    return `${label} unavailable${envelope.error_code ? ` · ${boundedText(String(envelope.error_code), 64)}` : ""}`;
  }
  const authority = boundedText(String(envelope.authority ?? "unknown"), 32).replaceAll("_", " ");
  return status === "stale"
    ? `Showing stale ${label.toLowerCase()} · ${authority}`
    : `Confirmed ${label.toLowerCase()} · ${authority}`;
}

function retainLastData(previous, incoming) {
  if (incoming?.status === "unavailable" && previous?.data) {
    return {
      ...previous,
      status: "stale",
      freshness: "stale",
      error_code: incoming.error_code,
      stale_error: true,
    };
  }
  return incoming;
}

function idSegment(value) {
  const id = shortIdentifier(value);
  return id ? encodeURIComponent(id) : "";
}

function pathForHostSessions(hostId) {
  const host = idSegment(hostId);
  return host ? `/dash/api/v1/hosts/${host}/sessions` : "";
}

function pathForSession(hostId, sessionId, suffix = "") {
  const host = idSegment(hostId);
  const session = idSegment(sessionId);
  return host && session ? `/dash/api/v1/hosts/${host}/sessions/${session}${suffix}` : "";
}

function startDashboard(documentRef = document, windowRef = window) {
  const byId = (id) => documentRef.getElementById(id);
  const selection = parseSelection(windowRef.location);
  const fence = new SelectionFence(selection.hostId, selection.sessionId);
  const requests = new RequestCoordinator();
  const state = {
    bootstrap: null,
    hostsEnvelope: null,
    hosts: [],
    sessionsEnvelope: null,
    sessionEnvelope: null,
    tasksEnvelope: null,
    contextEnvelope: null,
    timelineEnvelope: null,
    timelineEvents: [],
    timelineCursor: "",
    taskSnapshots: new Map(),
    degraded: new Map(),
    fence,
    refreshing: false,
    timer: null,
    lastUpdatedAt: null,
  };

  function selectedHost() {
    return state.hosts.find((host) => host?.host_id === fence.value.hostId) ?? null;
  }

  function updateDegraded(key, label, envelope) {
    if (envelope?.status === "unavailable") {
      state.degraded.set(key, `${label}: ${boundedText(String(envelope.error_code ?? "unavailable"), 64)}`);
    } else {
      state.degraded.delete(key);
    }
    renderDegraded();
  }

  function renderDegraded() {
    const banner = byId("degraded-banner");
    if (state.degraded.size === 0) {
      banner.hidden = true;
      setText(banner, "");
      return;
    }
    banner.hidden = false;
    setText(banner, `Some dashboard data is unavailable · ${[...state.degraded.values()].join(" · ")}`);
  }

  function setRefreshStatus(label, tone = "quiet") {
    setText(byId("refresh-label"), label);
    const dot = byId("refresh-dot");
    dot.className = `status-dot${tone === "quiet" ? " status-dot--quiet" : tone === "warning" ? " status-dot--warning" : tone === "error" ? " status-dot--error" : ""}`;
  }

  function renderBootstrap(envelope) {
    state.bootstrap = envelope;
    updateDegraded("bootstrap", "Fabric", envelope);
    if (!envelope || envelope.status === "unavailable" || !envelope.data) {
      if (!state.lastUpdatedAt) setRefreshStatus("Fabric unavailable", "error");
      return;
    }
    const data = envelope.data;
    setText(byId("deployment-value"), boundedText(String(data.deployment ?? "Unknown"), 128));
    setText(byId("version-value"), boundedText(String(data.version ?? data.deployment ?? "Unknown"), 128));
    const fingerprint = boundedText(String(data.contract_fingerprint ?? "Unknown"), 256);
    const fingerprintElement = byId("fingerprint-value");
    const shortFingerprint = fingerprint.length > 20
      ? `${fingerprint.slice(0, 10)}…${fingerprint.slice(-6)}`
      : fingerprint;
    setText(fingerprintElement, shortFingerprint, "Unknown", fingerprint);
    fingerprintElement.setAttribute("aria-label", `Contract fingerprint ${fingerprint}`);
    byId("refresh-button").disabled = false;
    if (state.lastUpdatedAt) setRefreshStatus("Connected", "live");
  }

  function hostAvailability(host) {
    const value = host?.availability;
    return value === "online" || value === "offline" || value === "unknown" || value === "unavailable"
      ? value
      : "unknown";
  }

  function hostHistoryText(host) {
    const history = host?.connection_history;
    if (!history || history.status === "unknown") return "Connection history unknown";
    if (history.status === "unavailable") return "Connection history unavailable";
    if (history.connected_at != null) return `Connected ${formatTime(history.connected_at)}`;
    if (history.last_seen != null) return `Last seen ${formatTime(history.last_seen)}`;
    return "Connection history unknown";
  }

  function hostReplicaText(host) {
    const replica = host?.replica;
    if (!replica || replica.status === "unknown") return "Last synchronized unknown";
    if (replica.status === "unavailable") return "Replica metadata unavailable";
    const synced = replica.last_synced_at == null ? "Last synchronized unknown" : `Synced ${formatTime(replica.last_synced_at)}`;
    const gap = Number.isFinite(replica.gap_count) && replica.gap_count > 0 ? ` · ${replica.gap_count} gap${replica.gap_count === 1 ? "" : "s"}` : "";
    const degraded = replica.journal_degraded ? " · degraded" : "";
    return `${synced}${gap}${degraded}`;
  }

  function renderSessionChoices(parent, hostId) {
    if (fence.value.hostId !== hostId) return;
    const envelope = state.sessionsEnvelope;
    if (!envelope || envelope.data?.host_id !== hostId) {
      appendText(documentRef, parent, "div", "component-state component-state--quiet", "Loading sessions…");
      return;
    }
    if (envelope.status === "unavailable" || !Array.isArray(envelope.data?.sessions)) {
      appendText(documentRef, parent, "div", "component-state component-state--error", componentMessage(envelope, "Sessions"));
      return;
    }
    const sessions = envelope.data.sessions.slice(0, MAX_SESSIONS);
    if (sessions.length === 0) {
      appendText(documentRef, parent, "div", "component-state component-state--quiet", "No sessions reported.");
      return;
    }
    for (const session of sessions) {
      const sessionId = shortIdentifier(session?.session_id);
      if (!sessionId) continue;
      const button = createElement(documentRef, "button", `session-choice${fence.value.sessionId === sessionId ? " session-choice--selected" : ""}`);
      button.type = "button";
      button.setAttribute("aria-pressed", String(fence.value.sessionId === sessionId));
      const name = appendText(documentRef, button, "span", "", sessionId);
      if (sessionId.length > 18) name.title = sessionId;
      appendText(documentRef, button, "span", "", boundedText(String(session.status ?? "unknown"), 32));
      button.addEventListener("click", () => select({ hostId, sessionId }, true));
      parent.append(button);
    }
  }

  function renderHosts() {
    const root = byId("host-list");
    root.replaceChildren();
    const envelope = state.hostsEnvelope;
    const components = envelope?.data?.components;
    const hostDataAvailable = Array.isArray(envelope?.data?.hosts);
    const snapshotUnavailable = !hostDataAvailable && envelope?.status === "unavailable";
    const hosts = snapshotUnavailable
      ? state.hosts.map((host) => ({ ...host, availability: "unknown", stale_liveness: true }))
      : state.hosts;

    if (!envelope) {
      setComponentState(byId("inventory-status"), "Loading host inventory…");
    } else if (envelope.status === "unavailable" && hosts.length === 0) {
      setComponentState(byId("inventory-status"), componentMessage(envelope, "Host inventory"), "error");
    } else if (components && Object.values(components).some((component) => component?.status === "unavailable")) {
      const failed = Object.entries(components)
        .filter(([, component]) => component?.status === "unavailable")
        .map(([name]) => name)
        .join(", ");
      setComponentState(byId("inventory-status"), `Partial inventory · unavailable: ${boundedText(failed, 100)}`, "stale");
    } else if (snapshotUnavailable) {
      setComponentState(byId("inventory-status"), "Showing the last inventory snapshot · live availability is unknown.", "stale");
    } else if (hosts.length === 0) {
      setComponentState(byId("inventory-status"), "No configured hosts.", "quiet");
    } else {
      const unknownLiveness = components?.liveness?.status === "unavailable";
      setComponentState(
        byId("inventory-status"),
        unknownLiveness ? "Inventory confirmed · current availability is unknown." : componentMessage(envelope, "Host inventory"),
        unknownLiveness ? "stale" : envelope.status === "stale" ? "stale" : "quiet",
      );
    }

    byId("host-count").textContent = snapshotUnavailable ? `${hosts.length}*` : String(hosts.length);
    for (const host of hosts.slice(0, MAX_HOSTS)) {
      const hostId = shortIdentifier(host?.host_id);
      if (!hostId) continue;
      const selected = fence.value.hostId === hostId;
      const card = createElement(documentRef, "article", `host-card${selected ? " host-card--selected" : ""}`);
      const button = createElement(documentRef, "button", "host-button");
      button.type = "button";
      button.setAttribute("aria-pressed", String(selected));
      const nameRow = createElement(documentRef, "span", "host-name-row");
      const name = appendText(documentRef, nameRow, "span", "host-name", hostId);
      if (hostId.length > 18) name.title = hostId;
      const availability = hostAvailability(host);
      appendText(documentRef, nameRow, "span", `availability availability--${availability}`, availability);
      button.append(nameRow);
      const chevron = appendText(documentRef, button, "span", "host-chevron", selected ? "⌄" : "›");
      chevron.setAttribute("aria-hidden", "true");
      button.addEventListener("click", () => select({ hostId, sessionId: "" }, true));
      card.append(button);

      const evidence = Array.isArray(host.evidence)
        ? host.evidence.slice(0, 4).map((item) => boundedText(String(item), 32)).join(" · ")
        : "Configured membership";
      const subline = createElement(documentRef, "div", "host-subline");
      appendText(documentRef, subline, "span", "", evidence || "Configured membership");
      card.append(subline);

      const history = appendText(documentRef, card, "div", "host-subline", hostHistoryText(host));
      history.title = history.textContent;
      const replica = appendText(documentRef, card, "div", "host-subline", hostReplicaText(host));
      replica.title = replica.textContent;

      if (selected) {
        const sessions = createElement(documentRef, "div", "host-sessions");
        renderSessionChoices(sessions, hostId);
        card.append(sessions);
      }
      root.append(card);
    }
  }

  async function loadBootstrap() {
    const envelope = await requests.request("/dash/api/v1/bootstrap", "bootstrap");
    renderBootstrap(envelope);
    return envelope;
  }

  async function loadHosts() {
    const envelope = await requests.request("/dash/api/v1/hosts", "hosts");
    state.hostsEnvelope = envelope;
    if (Array.isArray(envelope.data?.hosts)) {
      state.hosts = envelope.data.hosts.slice(0, MAX_HOSTS);
      const selectedHostExists = state.hosts.some((host) => host?.host_id === fence.value.hostId);
      if (!fence.value.hostId && state.hosts.length > 0) {
        select({ hostId: shortIdentifier(state.hosts[0]?.host_id), sessionId: "" }, true);
      } else if (fence.value.hostId && !selectedHostExists && state.hosts.length === 0) {
        // Keep an explicit URL selection so the scoped API can report its authorization result.
      }
    }
    updateDegraded("hosts", "Hosts", envelope);
    renderHosts();
    return envelope;
  }

  function writeSelectionToHash(value) {
    const params = new URLSearchParams();
    if (value.hostId) params.set("host", shortIdentifier(value.hostId));
    if (value.sessionId) params.set("session", shortIdentifier(value.sessionId));
    const nextHash = params.toString();
    if (windowRef.location.hash.slice(1) !== nextHash) windowRef.location.hash = nextHash;
  }

  function clearSessionViews() {
    state.sessionEnvelope = null;
    state.tasksEnvelope = null;
    state.contextEnvelope = null;
    state.timelineEnvelope = null;
    state.timelineEvents = [];
    state.timelineCursor = "";
    state.taskSnapshots.clear();
    state.degraded.delete("session");
    state.degraded.delete("tasks");
    state.degraded.delete("context");
    state.degraded.delete("timeline");
    byId("session-detail").hidden = true;
    setComponentState(byId("session-status"), fence.value.sessionId ? "Loading selected session…" : "Choose a session to inspect its current state.");
    setComponentState(byId("task-status"), "Select a session to load retained tasks.");
    setComponentState(byId("context-status"), "Select a session to inspect context.");
    setComponentState(byId("timeline-status"), "Select a session to load recent observations.");
    byId("task-list").replaceChildren();
    byId("session-facts").replaceChildren();
    byId("workspace-facts").replaceChildren();
    byId("context-detail").hidden = true;
    byId("timeline-list").replaceChildren();
    setText(byId("session-lifecycle"), fence.value.sessionId ? "Loading" : "No session selected");
    setText(byId("task-count"), "—");
    setText(byId("context-authority"), "—");
    setText(byId("timeline-authority"), "Replica");
    renderDegraded();
  }

  function select(value, writeHash = false) {
    const hostId = shortIdentifier(value.hostId);
    const sessionId = shortIdentifier(value.sessionId);
    const old = fence.snapshot();
    if (old.hostId === hostId && old.sessionId === sessionId) {
      if (writeHash) writeSelectionToHash({ hostId, sessionId });
      return;
    }
    const previousHostId = old.hostId;
    const next = fence.select(hostId, sessionId);
    requests.abortSelectionExcept(next.version);
    if (writeHash) writeSelectionToHash({ hostId, sessionId });

    if (previousHostId !== hostId) {
      state.sessionsEnvelope = null;
    }
    clearSessionViews();
    renderHosts();
    if (!hostId) return;

    if (previousHostId !== hostId) void loadSessions(next);
    if (sessionId) void loadSessionDetails(next);
  }

  async function loadSessions(snapshot) {
    if (!snapshot.hostId || !fence.matches(snapshot)) return;
    const path = pathForHostSessions(snapshot.hostId);
    if (!path) return;
    const envelope = await requests.request(path, `sessions:${snapshot.hostId}`, snapshot.version);
    if (!fence.matches(snapshot)) return;
    state.sessionsEnvelope = envelope;
    updateDegraded("sessions", "Sessions", envelope);
    renderHosts();
    const sessions = envelope.data?.sessions;
    if (Array.isArray(sessions) && sessions.length > 0 && !snapshot.sessionId) {
      const firstSessionId = shortIdentifier(sessions[0]?.session_id);
      if (firstSessionId) select({ hostId: snapshot.hostId, sessionId: firstSessionId }, true);
    } else if (snapshot.sessionId && Array.isArray(sessions)
      && !sessions.some((session) => session?.session_id === snapshot.sessionId)) {
      setComponentState(byId("session-status"), "This session is not in the selected host inventory.", "error");
    } else if (Array.isArray(sessions) && sessions.length === 0 && !snapshot.sessionId) {
      setComponentState(byId("session-status"), "No sessions reported for this host.", "quiet");
    }
    return envelope;
  }

  function selectedPaths(snapshot) {
    const sessionPath = pathForSession(snapshot.hostId, snapshot.sessionId);
    if (!sessionPath) return null;
    return {
      session: sessionPath,
      tasks: `${sessionPath}/tasks`,
      context: `${sessionPath}/context`,
      timeline: `${sessionPath}/timeline?limit=${MAX_TIMELINE_EVENTS}`,
    };
  }

  async function loadSessionDetails(snapshot) {
    if (!snapshot.hostId || !snapshot.sessionId || !fence.matches(snapshot)) return;
    byId("session-detail").hidden = false;
    const paths = selectedPaths(snapshot);
    if (!paths) return;
    const results = await Promise.all([
      requests.request(paths.session, `session:${snapshot.hostId}:${snapshot.sessionId}`, snapshot.version),
      requests.request(paths.tasks, `tasks:${snapshot.hostId}:${snapshot.sessionId}`, snapshot.version),
      requests.request(paths.context, `context:${snapshot.hostId}:${snapshot.sessionId}`, snapshot.version),
      requests.request(paths.timeline, `timeline:${snapshot.hostId}:${snapshot.sessionId}`, snapshot.version),
    ]);
    if (!fence.matches(snapshot)) return;
    const [sessionEnvelope, tasksEnvelope, contextEnvelope, timelineEnvelope] = results;
    state.sessionEnvelope = sessionEnvelope;
    state.tasksEnvelope = tasksEnvelope;
    state.contextEnvelope = contextEnvelope;
    state.timelineEnvelope = timelineEnvelope;
    state.timelineEvents = Array.isArray(timelineEnvelope.data?.events)
      ? timelineEnvelope.data.events.slice(-MAX_TIMELINE_EVENTS)
      : [];
    state.timelineCursor = boundedText(String(timelineEnvelope.data?.next_cursor ?? ""), 512);
    updateDegraded("session", "Session detail", sessionEnvelope);
    updateDegraded("tasks", "Tasks", tasksEnvelope);
    updateDegraded("context", "Context", contextEnvelope);
    updateDegraded("timeline", "Timeline", timelineEnvelope);
    renderSession(sessionEnvelope);
    renderTasks(tasksEnvelope);
    renderContext(contextEnvelope);
    renderTimeline(timelineEnvelope);
    renderHosts();
    return results;
  }

  function addFact(parent, label, value) {
    const fact = createElement(documentRef, "div", "fact");
    appendText(documentRef, fact, "span", "fact-label", label);
    const valueElement = appendText(documentRef, fact, "span", "fact-value", value == null || value === "" ? "Unknown" : boundedText(String(value)));
    if (typeof value === "string" && value.length > 18) valueElement.title = boundedText(value);
    parent.append(fact);
  }

  function sessionLifecycleClass(status) {
    const normalized = String(status ?? "unknown").toLowerCase();
    if (normalized === "active" || normalized === "running") return "active";
    if (["starting", "stopping", "degraded", "unknown"].includes(normalized)) return "warning";
    if (["crashed", "failed"].includes(normalized)) return "error";
    if (normalized === "stopped") return "muted";
    return "muted";
  }

  function renderSession(envelope) {
    const hostId = fence.value.hostId;
    const sessionId = fence.value.sessionId;
    const host = selectedHost();
    setText(byId("selected-host-label"), hostId ? `HOST · ${hostId}` : "SELECT A HOST");
    if (!sessionId) {
      byId("session-detail").hidden = true;
      setComponentState(byId("session-status"), hostId ? "Choose a session to inspect its current state." : "Choose a host to inspect its sessions.", "quiet");
      setText(byId("session-lifecycle"), "No session selected");
      return;
    }
    if (!envelope || envelope.status === "unavailable" || !envelope.data?.session) {
      byId("session-detail").hidden = true;
      setComponentState(byId("session-status"), componentMessage(envelope, "Session detail"), "error");
      setText(byId("session-lifecycle"), "Unavailable");
      return;
    }
    const data = envelope.data;
    if (data.host_id !== hostId || data.session_id !== sessionId) {
      byId("session-detail").hidden = true;
      setComponentState(byId("session-status"), "The returned session identity did not match the current selection.", "error");
      setText(byId("session-lifecycle"), "Identity mismatch");
      return;
    }

    const session = data.session;
    const status = boundedText(String(session.status ?? "unknown"), 32);
    const hostOffline = host && hostAvailability(host) !== "online";
    setComponentState(
      byId("session-status"),
      hostOffline ? `Last reported session projection · host availability ${hostAvailability(host)}.` : componentMessage(envelope, "Session detail"),
      hostOffline || envelope.status === "stale" ? "stale" : "quiet",
    );
    const lifecycle = byId("session-lifecycle");
    const lifecycleLabel = hostOffline ? `Last reported · ${status}` : status;
    setText(lifecycle, lifecycleLabel);
    lifecycle.className = `state-badge state-badge--${hostOffline ? "warning" : sessionLifecycleClass(status)}`;

    const facts = byId("session-facts");
    facts.replaceChildren();
    addFact(facts, "Lifecycle", hostOffline ? `Last reported ${status}` : status);
    addFact(facts, "Permission mode", session.permission_mode ?? (session.yolo ? "yolo" : "unknown"));
    addFact(facts, "Started", formatTime(session.started_at));
    addFact(facts, "Session ID", sessionId);

    const workspace = session.workspace && typeof session.workspace === "object" ? session.workspace : {};
    const workspaceFacts = byId("workspace-facts");
    workspaceFacts.replaceChildren();
    addFact(workspaceFacts, "Workspace type", workspace.workspace_type ?? "Unknown");
    addFact(workspaceFacts, "Repository", workspace.repository ?? "Unknown");
    addFact(workspaceFacts, "Branch / bookmark", workspace.branch ?? "Unknown");
    addFact(workspaceFacts, "Current task", workspace.task ?? "Unknown");
    byId("session-detail").hidden = false;
  }

  function renderTaskStatusBadge(parent, value, hostOffline) {
    const status = boundedText(String(value ?? "unknown"), 32).toLowerCase();
    const knownGood = ["completed", "succeeded", "succeeded_with_warnings"].includes(status);
    const knownBad = ["failed", "cancelled", "canceled", "blocked"].includes(status);
    const kind = hostOffline ? "warning" : knownGood ? "confirmed" : knownBad ? "error" : "muted";
    appendText(documentRef, parent, "span", `state-badge state-badge--${kind}`, hostOffline ? `Last retained · ${status}` : status);
  }

  function renderPendingSummary(parent, summary) {
    const pending = pendingForDisplay(summary);
    const stateName = pending.display_state;
    const line = createElement(documentRef, "div", `pending-summary${stateName === "pending" ? " pending-summary--pending" : ["unknown", "unavailable"].includes(stateName) ? ` pending-summary--${stateName}` : ""}`);
    const labels = {
      none: "No pending interaction",
      pending: "Interaction pending",
      unknown: "Interaction state unknown",
      unsupported: "Unsupported by host",
      unavailable: pending.expired ? "Summary expired · unavailable" : "Interaction summary unavailable",
    };
    appendText(documentRef, line, "span", "pending-icon", stateName === "pending" ? "!" : stateName === "none" ? "✓" : "·");
    appendText(documentRef, line, "strong", "", labels[stateName]);
    if (stateName === "pending" && Number.isFinite(pending.count)) {
      appendText(documentRef, line, "span", "", `${Math.max(0, Math.min(64, Math.trunc(pending.count)))} pending`);
    }
    if (stateName === "pending" && Array.isArray(pending.types)) {
      for (const type of pending.types.slice(0, 4)) {
        if (PENDING_TYPES.has(type)) appendText(documentRef, line, "span", "pending-type", type);
      }
    }
    if (pending.observed_at != null) {
      appendText(documentRef, line, "span", "", `Observed ${formatTime(pending.observed_at)}`);
    }
    if (pending.truncated === true) appendText(documentRef, line, "span", "", "Summary capped");
    if (pending.producer_kind) {
      appendText(documentRef, line, "span", "", boundedText(String(pending.producer_kind).replaceAll("_", " "), 48));
    }
    parent.append(line);
  }

  function renderTasks(envelope) {
    const root = byId("task-list");
    root.replaceChildren();
    const hostId = fence.value.hostId;
    const sessionId = fence.value.sessionId;
    if (!sessionId) {
      setComponentState(byId("task-status"), "Select a session to load retained tasks.", "quiet");
      setText(byId("task-count"), "—");
      return;
    }
    if (!envelope || envelope.status === "unavailable" || !Array.isArray(envelope.data?.backends)) {
      setComponentState(byId("task-status"), componentMessage(envelope, "Task list"), "error");
      setText(byId("task-count"), "?");
      return;
    }
    if (envelope.data.host_id !== hostId || envelope.data.session_id !== sessionId) {
      setComponentState(byId("task-status"), "Task data did not match the current selection.", "error");
      setText(byId("task-count"), "?");
      return;
    }

    const hostOffline = hostAvailability(selectedHost()) !== "online";
    const backends = envelope.data.backends.slice(0, 16);
    let taskCount = 0;
    let failedBackends = 0;
    state.taskSnapshots = new Map();
    const snapshotPrefix = `${hostId}\u0000${sessionId}\u0000`;
    for (const backend of backends) {
      const backendName = boundedText(String(backend?.backend ?? "unknown"), 48);
      const group = createElement(documentRef, "section", "backend-group");
      const heading = createElement(documentRef, "div", "backend-heading");
      appendText(documentRef, heading, "h3", "", backendName);
      const backendStatus = boundedText(String(backend?.status ?? "unavailable"), 32);
      appendText(documentRef, heading, "span", `backend-state backend-state--${backendStatus === "confirmed" ? "confirmed" : "unavailable"}`, backendStatus);
      group.append(heading);
      if (backendStatus !== "confirmed" || !Array.isArray(backend.tasks)) {
        failedBackends += 1;
        appendText(documentRef, group, "div", "component-state component-state--error", `Task store unavailable${backend.error_code ? ` · ${boundedText(String(backend.error_code), 64)}` : ""}`);
        root.append(group);
        continue;
      }
      const tasks = backend.tasks.slice(0, MAX_TASKS);
      if (tasks.length === 0) {
        appendText(documentRef, group, "div", "component-state component-state--quiet", "No retained tasks recorded.");
      }
      for (const incoming of tasks) {
        const taskId = shortIdentifier(incoming?.task_id);
        if (!taskId) continue;
        taskCount += 1;
        const snapshotKey = `${snapshotPrefix}${backendName}\u0000${taskId}`;
        const task = mergeTaskProjection(state.taskSnapshots.get(snapshotKey), incoming);
        state.taskSnapshots.set(snapshotKey, task);
        const card = createElement(documentRef, "article", "task-card");
        const top = createElement(documentRef, "div", "task-card-top");
        const identity = createElement(documentRef, "div", "task-identity");
        const displayId = taskId.length > 28 ? `${taskId.slice(0, 18)}…${taskId.slice(-6)}` : taskId;
        const id = appendText(documentRef, identity, "span", "task-id", displayId);
        id.title = taskId;
        appendText(documentRef, identity, "span", "task-backend", backendName);
        top.append(identity);
        renderTaskStatusBadge(top, task.status, hostOffline);
        card.append(top);
        const meta = createElement(documentRef, "div", "task-meta");
        appendText(documentRef, meta, "span", "", `Task revision ${boundedText(String(task.revision ?? "unknown"), 32)}`);
        appendText(documentRef, meta, "span", "", `Updated ${formatTime(task.last_updated_at)}`);
        appendText(documentRef, meta, "span", "", "Retained projection");
        card.append(meta);
        renderPendingSummary(card, task.pending_interaction);
        group.append(card);
      }
      root.append(group);
    }
    setText(byId("task-count"), failedBackends > 0 ? `${taskCount} + ?` : String(taskCount));
    const message = hostOffline
      ? "Retained task records · host availability is not live, so task states are last known."
      : failedBackends > 0
        ? `Partial task list · ${failedBackends} backend store${failedBackends === 1 ? "" : "s"} unavailable.`
        : "Read-only retained task projection. Pending summaries expire unless refreshed by their producer.";
    setComponentState(byId("task-status"), message, hostOffline || failedBackends > 0 || envelope.status === "stale" ? "stale" : "quiet");
  }
