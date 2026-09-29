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
  const date = new Date(timestamp);
  if (!Number.isFinite(date.getTime())) return "Unknown";
  return new Intl.DateTimeFormat(undefined, {
    dateStyle: "medium",
    timeStyle: "short",
  }).format(new Date(timestamp));
}

function sequenceValue(value) {
  if (typeof value === "number") {
    if (!Number.isSafeInteger(value) || value < 0) return null;
    return BigInt(value);
  }
  if (typeof value === "string" && /^(0|[1-9]\d{0,19})$/.test(value)) {
    try {
      const parsed = BigInt(value);
      return parsed <= 18_446_744_073_709_551_615n ? parsed : null;
    } catch {
      return null;
    }
  }
  return null;
}

function compareSequence(left, right) {
  const leftValue = sequenceValue(left);
  const rightValue = sequenceValue(right);
  if (leftValue == null && rightValue == null) return left == null && right == null ? 0 : null;
  if (leftValue == null || rightValue == null) return null;
  return leftValue === rightValue ? 0 : leftValue < rightValue ? -1 : 1;
}

function samePendingContent(left, right) {
  const leftTypes = Array.isArray(left?.types) ? left.types.filter((type) => PENDING_TYPES.has(type)) : [];
  const rightTypes = Array.isArray(right?.types) ? right.types.filter((type) => PENDING_TYPES.has(type)) : [];
  return left?.state === right?.state
    && left?.count === right?.count
    && left?.truncated === right?.truncated
    && JSON.stringify(leftTypes) === JSON.stringify(rightTypes);
}

function mergePendingSummary(previous, incoming, taskOrder = 0) {
  if (!previous) return incoming;
  if (!incoming) return previous;
  const previousSupported = PENDING_STATES.has(previous.state) && previous.state !== "unsupported";
  const incomingSupported = PENDING_STATES.has(incoming.state) && incoming.state !== "unsupported";
  if (incoming.state === "unsupported") {
    return (taskOrder == null || taskOrder < 0) && previousSupported ? previous : incoming;
  }
  if (!previousSupported) return incomingSupported && taskOrder >= 0 ? incoming : previous;

  const previousKind = previous.producer_kind;
  const incomingKind = incoming.producer_kind;
  if (previousKind && incomingKind && previousKind !== incomingKind) {
    return taskOrder > 0 && incomingSupported ? incoming : previous;
  }

  const previousEpoch = previous.producer_epoch;
  const incomingEpoch = incoming.producer_epoch;
  if (previousEpoch != null && incomingEpoch != null) {
    const epochOrder = compareSequence(incomingEpoch, previousEpoch);
    if (epochOrder == null) return previous;
    if (epochOrder < 0) return previous;
    if (epochOrder > 0) return incoming;
  } else if (previousEpoch != null && incomingEpoch == null) {
    return previous;
  } else if (previousEpoch == null && incomingEpoch != null && taskOrder < 0) {
    return previous;
  }

  const previousRevision = previous.summary_revision;
  const incomingRevision = incoming.summary_revision;
  if (previousRevision != null && incomingRevision != null) {
    const revisionOrder = compareSequence(incomingRevision, previousRevision);
    if (revisionOrder == null) return previous;
    if (revisionOrder < 0) return previous;
    if (revisionOrder > 0) return incoming;
  } else if (previousRevision != null && incomingRevision == null) {
    return previous;
  } else if (previousRevision == null && incomingRevision != null && taskOrder < 0) {
    return previous;
  }

  const sameObservedAt = Number.isFinite(parseTimestampMs(previous.observed_at))
    && parseTimestampMs(previous.observed_at) === parseTimestampMs(incoming.observed_at);
  const sameExpiresAt = Number.isFinite(parseTimestampMs(previous.expires_at))
    && parseTimestampMs(previous.expires_at) === parseTimestampMs(incoming.expires_at);
  const sameProducerEpoch = previousEpoch != null
    && incomingEpoch != null
    && compareSequence(incomingEpoch, previousEpoch) === 0;
  const sameProducerKind = Boolean(previousKind && incomingKind && previousKind === incomingKind);
  const sameSummaryRevision = previousRevision != null
    && incomingRevision != null
    && compareSequence(incomingRevision, previousRevision) === 0;
  const expiredProjection = incoming.state === "unavailable"
    && sameProducerKind
    && sameProducerEpoch
    && sameSummaryRevision
    && sameObservedAt
    && sameExpiresAt
    && incoming.count == null
    && (!Array.isArray(incoming.types) || incoming.types.length === 0);
  if (expiredProjection) return incoming;

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
  const task = taskOrder == null || taskOrder < 0 ? previous : incoming;
  const summary = mergePendingSummary(
    previous.pending_interaction ?? { state: "unsupported" },
    incoming.pending_interaction ?? { state: "unsupported" },
    taskOrder,
  );
  return { ...task, pending_interaction: summary };
}

function pendingForDisplay(summary, nowMs = Date.now()) {
  const value = summary && typeof summary === "object" ? summary : { state: "unsupported" };
  const state = PENDING_STATES.has(value.state) ? value.state : "unknown";
  if (state === "unsupported") return { ...value, display_state: state };
  const expiresAt = parseTimestampMs(value.expires_at);
  if (!Number.isFinite(expiresAt) || !Number.isFinite(new Date(expiresAt).getTime()) || expiresAt <= nowMs) {
    return { ...value, display_state: "unavailable", expired: true };
  }
  return { ...value, display_state: state, expired: false };
}

function nextPendingExpiryMs(tasks, nowMs = Date.now()) {
  let next = Infinity;
  if (!Array.isArray(tasks)) return null;
  for (const task of tasks) {
    const pending = task?.pending_interaction;
    if (!pending || pending.state === "unsupported") continue;
    const expiresAt = parseTimestampMs(pending.expires_at);
    if (Number.isFinite(expiresAt)
      && Number.isFinite(new Date(expiresAt).getTime())
      && expiresAt > nowMs
      && expiresAt < next) next = expiresAt;
  }
  return Number.isFinite(next) ? next : null;
}

function isSessionProjectionCurrent(envelope, hostAvailability) {
  return envelope?.status === "confirmed" && hostAvailability === "online";
}

function isTaskProjectionCurrent(envelopeStatus, backendStatus, hostAvailability) {
  return envelopeStatus === "confirmed" && backendStatus === "confirmed" && hostAvailability === "online";
}

function isContextResolverCurrent(envelope, resolver, hostAvailability) {
  return envelope?.status === "confirmed"
    && resolver?.status === "confirmed"
    && Boolean(resolver?.data)
    && resolver?.authority === "host_live"
    && hostAvailability === "online";
}

function effectiveHostAvailability(host, inventoryEnvelope, browserOffline = false) {
  if (browserOffline) return "unknown";
  const livenessConfirmed = Array.isArray(inventoryEnvelope?.data?.hosts)
    && inventoryEnvelope?.data?.components?.liveness?.status === "confirmed";
  if (!livenessConfirmed) return "unknown";
  const value = host?.availability;
  return value === "online" || value === "offline" || value === "unknown" || value === "unavailable"
    ? value
    : "unknown";
}

function membershipRemovalConfirmed(envelope, selectedHostId) {
  if (!selectedHostId
    || envelope?.data?.components?.membership?.status !== "confirmed"
    || !Array.isArray(envelope?.data?.hosts)) return false;
  return !envelope.data.hosts.some((host) => host?.host_id === selectedHostId);
}

function hasUnavailableChild(value, depth = 0) {
  if (depth > 6 || value == null || typeof value !== "object") return false;
  if (value.status === "unavailable") return true;
  const children = Array.isArray(value) ? value : Object.values(value);
  return children.some((child) => hasUnavailableChild(child, depth + 1));
}

function hasPartialList(value, depth = 0) {
  if (depth > 6 || value == null || typeof value !== "object") return false;
  if (value.truncated === true || (Number.isSafeInteger(value.truncated) && value.truncated > 0)
    || (Number.isSafeInteger(value.skipped) && value.skipped > 0)) return true;
  const children = Array.isArray(value) ? value : Object.values(value);
  return children.some((child) => hasPartialList(child, depth + 1));
}

function responseIsDegraded(envelope) {
  return envelope?.status === "unavailable"
    || Boolean(envelope?.stale_error)
    || hasUnavailableChild(envelope?.data)
    || hasPartialList(envelope?.data);
}

function taskBackendPresentation(envelopeStatus, backend, hostAvailability) {
  const backendStatus = String(backend?.status ?? "unavailable");
  const tasks = Array.isArray(backend?.tasks) ? backend.tasks : null;
  const skipped = Number.isSafeInteger(backend?.skipped) && backend.skipped > 0 ? backend.skipped : 0;
  const truncated = backend?.truncated === true || (Number.isSafeInteger(backend?.truncated) && backend.truncated > 0);
  const total = Number.isSafeInteger(backend?.total) && backend.total >= 0 ? backend.total : null;
  const partial = skipped > 0 || truncated || (total != null && tasks != null && total > tasks.length);
  const current = isTaskProjectionCurrent(envelopeStatus, backendStatus, hostAvailability) && !partial;
  const available = backendStatus === "confirmed"
    && tasks !== null
    && hostAvailability === "online"
    && envelopeStatus !== "unavailable";
  return { backendStatus, tasks, skipped, truncated, total, partial, current, available };
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

  invalidate() {
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

function markEnvelopeStale(envelope, reason = "stale") {
  if (!envelope?.data || envelope.status === "unavailable") return envelope;
  return { ...envelope, status: "stale", freshness: "stale", stale_reason: boundedText(reason, 64) };
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
    backendSnapshots: new Map(),
    degraded: new Map(),
    fence,
    refreshing: false,
    timer: null,
    pendingTimer: null,
    browserOffline: windowRef.navigator?.onLine === false,
    lastUpdatedAt: null,
  };

  function selectedHost() {
    const host = state.hosts.find((candidate) => candidate?.host_id === fence.value.hostId);
    if (!host) return null;
    const availability = effectiveHostAvailability(host, state.hostsEnvelope, state.browserOffline);
    return { ...host, availability, stale_liveness: availability === "unknown" };
  }

  function updateDegraded(key, label, envelope) {
    if (responseIsDegraded(envelope)) {
      const detail = envelope?.error_code
        ?? findUnavailableError(envelope?.data)
        ?? (hasPartialList(envelope?.data) ? "partial list" : "component unavailable");
      state.degraded.set(key, `${label}: ${boundedText(String(detail), 64)}`);
    } else {
      state.degraded.delete(key);
    }
    renderDegraded();
  }

  function findUnavailableError(value, depth = 0) {
    if (depth > 6 || value == null || typeof value !== "object") return "";
    if (value.status === "unavailable") return boundedText(String(value.error_code ?? "component unavailable"), 64);
    const children = Array.isArray(value) ? value : Object.values(value);
    for (const child of children) {
      const found = findUnavailableError(child, depth + 1);
      if (found) return found;
    }
    return "";
  }

  function renderDegraded() {
    const banner = byId("degraded-banner");
    if (state.degraded.size === 0) {
      banner.hidden = true;
      setText(banner, "");
      return;
    }
    banner.hidden = false;
    setText(banner, `Some dashboard data is degraded or unavailable · ${[...state.degraded.values()].join(" · ")}`);
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
    if (!Array.isArray(envelope.data?.sessions)) {
      appendText(documentRef, parent, "div", "component-state component-state--error", componentMessage(envelope, "Sessions"));
      return;
    }
    const allSessions = envelope.data.sessions;
    const sessions = allSessions.slice(0, MAX_SESSIONS);
    if (envelope.data.truncated === true || allSessions.length > sessions.length) {
      appendText(documentRef, parent, "div", "component-state component-state--stale", "Session list capped at the displayed items.");
    }
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
      const host = selectedHost();
      const lastReported = hostAvailability(host) !== "online" || envelope.status === "stale";
      appendText(documentRef, button, "span", "", `${lastReported ? "Last · " : ""}${boundedText(String(session.status ?? "unknown"), 32)}`);
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
    const snapshotUnavailable = !hostDataAvailable && envelope != null;
    const hosts = snapshotUnavailable || state.browserOffline
      ? state.hosts.map((host) => ({ ...host, availability: "unknown", stale_liveness: true }))
      : state.hosts;

    if (!envelope) {
      setComponentState(byId("inventory-status"), "Loading host inventory…");
    } else if (snapshotUnavailable && hosts.length === 0) {
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
        ? host.evidence.slice(0, 4).map((item) => boundedText(String(item).replaceAll("_", " "), 32)).join(" · ")
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
    let envelope = await requests.request("/dash/api/v1/hosts", "hosts");
    if (envelope.status !== "unavailable" && !Array.isArray(envelope.data?.hosts)) {
      envelope = unavailableEnvelope("invalid_response");
    }
    state.hostsEnvelope = envelope;
    if (Array.isArray(envelope.data?.hosts)) {
      state.hosts = envelope.data.hosts.slice(0, MAX_HOSTS);
      const selectedHostExists = state.hosts.some((host) => host?.host_id === fence.value.hostId);
      if (!fence.value.hostId && state.hosts.length > 0) {
        select({ hostId: shortIdentifier(state.hosts[0]?.host_id), sessionId: "" }, true);
      } else if (membershipRemovalConfirmed(envelope, fence.value.hostId) && !selectedHostExists) {
        const current = fence.invalidate();
        requests.abortSelectionExcept(current.version);
        state.sessionsEnvelope = null;
        clearSessionViews();
      }
    }
    updateDegraded("hosts", "Hosts", envelope);
    renderHosts();
    renderSession(state.sessionEnvelope);
    renderTasks(state.tasksEnvelope);
    renderContext(state.contextEnvelope);
    renderTimeline(state.timelineEnvelope);
    if (membershipRemovalConfirmed(envelope, fence.value.hostId)) {
      void loadSessions(fence.snapshot());
    }
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
    clearTimeout(state.pendingTimer);
    state.pendingTimer = null;
    state.sessionEnvelope = null;
    state.tasksEnvelope = null;
    state.contextEnvelope = null;
    state.timelineEnvelope = null;
    state.timelineEvents = [];
    state.timelineCursor = "";
    state.taskSnapshots.clear();
    state.backendSnapshots.clear();
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
    byId("provenance-block").replaceChildren();
    byId("provenance-block").hidden = true;
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
      state.degraded.delete("sessions");
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
    state.sessionsEnvelope = retainLastData(state.sessionsEnvelope, envelope);
    updateDegraded("sessions", "Sessions", envelope);
    renderHosts();
    if (!Array.isArray(envelope.data?.sessions)) {
      setComponentState(byId("session-status"), componentMessage(envelope, "Sessions"), "error");
    }
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
    const after = state.timelineCursor;
    const timelineQuery = new URLSearchParams({ limit: String(MAX_TIMELINE_EVENTS) });
    if (after) timelineQuery.set("after", after);
    return {
      session: sessionPath,
      tasks: `${sessionPath}/tasks`,
      context: `${sessionPath}/context`,
      timeline: `${sessionPath}/timeline?${timelineQuery.toString()}`,
      timelineAfter: after,
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
    const [sessionResponse, tasksResponse, contextResponse, timelineResponse] = results;
    const sessionEnvelope = retainLastData(state.sessionEnvelope, sessionResponse);
    const tasksEnvelope = retainLastData(state.tasksEnvelope, tasksResponse);
    const contextEnvelope = retainLastData(state.contextEnvelope, contextResponse);
    const timelineEnvelope = retainLastData(state.timelineEnvelope, timelineResponse);
    state.sessionEnvelope = sessionEnvelope;
    state.tasksEnvelope = tasksEnvelope;
    state.contextEnvelope = contextEnvelope;
    state.timelineEnvelope = timelineEnvelope;
    if (Array.isArray(timelineEnvelope.data?.events)) {
      const incomingEvents = timelineEnvelope.data.events.slice(-MAX_TIMELINE_EVENTS);
      if (paths.timelineAfter) {
        const combined = [...state.timelineEvents, ...incomingEvents];
        const unique = new Map();
        for (const event of combined) {
          const key = `${event?.cloud_seq ?? event?.source_revision ?? event?.observed_at ?? ""}\u0000${event?.kind ?? ""}\u0000${event?.action ?? ""}\u0000${event?.target_backend ?? ""}`;
          unique.set(key, event);
        }
        state.timelineEvents = [...unique.values()].slice(-MAX_TIMELINE_EVENTS);
      } else {
        state.timelineEvents = incomingEvents;
      }
    }
    if (timelineEnvelope.data?.next_cursor) {
      state.timelineCursor = boundedText(String(timelineEnvelope.data.next_cursor), 512);
    }
    updateDegraded("session", "Session detail", sessionResponse);
    updateDegraded("tasks", "Tasks", tasksResponse);
    updateDegraded("context", "Context", contextResponse);
    updateDegraded("timeline", "Timeline", timelineResponse);
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
    if (!envelope || !envelope.data?.session) {
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
    const sessionStale = !isSessionProjectionCurrent(envelope, hostAvailability(host));
    setComponentState(
      byId("session-status"),
      hostOffline ? `Last reported session projection · host availability ${hostAvailability(host)}.` : componentMessage(envelope, "Session detail"),
      sessionStale ? "stale" : "quiet",
    );
    const lifecycle = byId("session-lifecycle");
    const lifecycleLabel = sessionStale ? `Last reported · ${status}` : status;
    setText(lifecycle, lifecycleLabel);
    lifecycle.className = `state-badge state-badge--${sessionStale ? "warning" : sessionLifecycleClass(status)}`;

    const facts = byId("session-facts");
    facts.replaceChildren();
    addFact(facts, "Lifecycle", sessionStale ? `Last reported ${status}` : status);
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

  function renderTaskStatusBadge(parent, value, staleProjection) {
    const status = boundedText(String(value ?? "unknown"), 32).toLowerCase();
    const knownGood = ["completed", "succeeded", "succeeded_with_warnings"].includes(status);
    const knownBad = ["failed", "cancelled", "canceled", "blocked"].includes(status);
    const kind = staleProjection ? "warning" : knownGood ? "confirmed" : knownBad ? "error" : "muted";
    appendText(documentRef, parent, "span", `state-badge state-badge--${kind}`, staleProjection ? `Last retained · ${status}` : status);
  }

  function renderPendingSummary(parent, summary, staleProjection = false) {
    const pending = pendingForDisplay(summary);
    const stateName = pending.display_state;
    const line = createElement(documentRef, "div", `pending-summary${stateName === "pending" ? " pending-summary--pending" : ["unknown", "unavailable"].includes(stateName) ? ` pending-summary--${stateName}` : ""}`);
    const labels = {
      none: staleProjection ? "Last observed · no pending interaction" : "No pending interaction",
      pending: staleProjection ? "Last observed · interaction pending" : "Interaction pending",
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
      clearTimeout(state.pendingTimer);
      state.pendingTimer = null;
      setComponentState(byId("task-status"), "Select a session to load retained tasks.", "quiet");
      setText(byId("task-count"), "—");
      return;
    }
    if (!envelope || !Array.isArray(envelope.data?.backends)) {
      clearTimeout(state.pendingTimer);
      state.pendingTimer = null;
      setComponentState(byId("task-status"), componentMessage(envelope, "Task list"), "error");
      setText(byId("task-count"), "?");
      return;
    }
    if (envelope.data.host_id !== hostId || envelope.data.session_id !== sessionId) {
      clearTimeout(state.pendingTimer);
      state.pendingTimer = null;
      setComponentState(byId("task-status"), "Task data did not match the current selection.", "error");
      setText(byId("task-count"), "?");
      return;
    }

    const hostOffline = hostAvailability(selectedHost()) !== "online";
    const backends = envelope.data.backends.slice(0, 16);
    let taskCount = 0;
    let failedBackends = 0;
    let partialBackends = 0;
    const snapshotPrefix = `${hostId}\u0000${sessionId}\u0000`;
    for (const backend of backends) {
      const backendName = boundedText(String(backend?.backend ?? "unknown"), 48);
      const backendCacheKey = `${snapshotPrefix}${backendName}`;
      const group = createElement(documentRef, "section", "backend-group");
      const heading = createElement(documentRef, "div", "backend-heading");
      appendText(documentRef, heading, "h3", "", backendName);
      const backendLiveness = hostAvailability(selectedHost());
      const presentation = taskBackendPresentation(envelope.status, backend, backendLiveness);
      const { backendStatus, skipped, truncated: backendTruncated, total: reportedTotal } = presentation;
      const backendPartial = presentation.partial;
      const backendAvailable = presentation.available;
      const backendCurrent = presentation.current;
      appendText(
        documentRef,
        heading,
        "span",
        `backend-state backend-state--${backendStatus === "confirmed" ? (backendPartial || !backendCurrent ? "partial" : "confirmed") : "unavailable"}`,
        backendPartial || !backendCurrent ? "partial" : backendStatus,
      );
      group.append(heading);
      const priorBackend = state.backendSnapshots.get(backendCacheKey);
      if (backendAvailable) state.backendSnapshots.set(backendCacheKey, backend);
      const sourceBackend = backendAvailable ? { ...backend, tasks: presentation.tasks } : priorBackend;
      const staleProjection = !backendCurrent || backendPartial;
      if (!backendAvailable) {
        if (backendStatus !== "confirmed" || !Array.isArray(backend.tasks)) failedBackends += 1;
        else partialBackends += 1;
        appendText(
          documentRef,
          group,
          "div",
          `component-state ${priorBackend ? "component-state--stale" : "component-state--error"}`,
          priorBackend
            ? `Showing last backend snapshot${backend.error_code ? ` · ${boundedText(String(backend.error_code), 64)}` : ""}.`
            : backendStatus !== "confirmed"
              ? `Task store unavailable${backend.error_code ? ` · ${boundedText(String(backend.error_code), 64)}` : ""}`
              : `Task status not confirmed · host availability ${backendLiveness}.`,
        );
      }
      if (backendAvailable && backendPartial) {
        partialBackends += 1;
        const details = [
          skipped > 0 && `${skipped} skipped`,
          backendTruncated && "list capped",
          reportedTotal != null && reportedTotal > backend.tasks.length && `showing ${backend.tasks.length} of ${reportedTotal}`,
        ].filter(Boolean).join(" · ");
        appendText(documentRef, group, "div", "component-state component-state--stale", `Partial backend list${details ? ` · ${details}` : ""}.`);
      }
      if (!sourceBackend || !Array.isArray(sourceBackend.tasks)) {
        root.append(group);
        continue;
      }
      const tasks = sourceBackend.tasks.slice(0, MAX_TASKS);
      if (sourceBackend.tasks.length > MAX_TASKS) {
        partialBackends += 1;
        appendText(documentRef, group, "div", "component-state component-state--stale", `Task cards capped at ${MAX_TASKS}.`);
      }
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
        renderTaskStatusBadge(top, task.status, staleProjection);
        card.append(top);
        const meta = createElement(documentRef, "div", "task-meta");
        appendText(documentRef, meta, "span", "", `Task revision ${boundedText(String(task.revision ?? "unknown"), 32)}`);
        appendText(documentRef, meta, "span", "", `Updated ${formatTime(task.last_updated_at)}`);
        appendText(documentRef, meta, "span", "", "Retained projection");
        card.append(meta);
        renderPendingSummary(card, task.pending_interaction, staleProjection);
        group.append(card);
      }
      root.append(group);
    }
    setText(byId("task-count"), failedBackends > 0 ? `${taskCount} + ?` : partialBackends > 0 ? `${taskCount} partial` : String(taskCount));
    const message = hostOffline
      ? "Retained task records · host availability is not live, so task states are last known."
      : failedBackends > 0 || partialBackends > 0
        ? `Partial task list · ${failedBackends} backend store${failedBackends === 1 ? "" : "s"} unavailable · ${partialBackends} incomplete.`
        : envelope.status === "stale"
          ? "Showing last confirmed task projection · states and pending summaries are not current."
          : "Read-only retained task projection. Pending summaries expire unless refreshed by their producer.";
    setComponentState(byId("task-status"), message, hostOffline || failedBackends > 0 || envelope.status !== "confirmed" ? "stale" : "quiet");
    schedulePendingExpiry();
  }

  function schedulePendingExpiry() {
    clearTimeout(state.pendingTimer);
    state.pendingTimer = null;
    const nextExpiry = nextPendingExpiryMs([...state.taskSnapshots.values()]);
    if (nextExpiry == null) return;
    const delay = Math.min(Math.max(0, nextExpiry - Date.now()), 2_147_000_000);
    state.pendingTimer = setTimeout(() => {
      state.pendingTimer = null;
      renderTasks(state.tasksEnvelope);
    }, delay);
  }

  function replicaFreshnessText(replica) {
    if (!replica || replica.status === "unknown") return "Replica freshness unknown";
    if (replica.status === "unavailable") return "Replica freshness unavailable";
    if (replica.status !== "confirmed" && replica.last_synced_at == null
      && replica.source_head_revision == null && replica.acked_through_revision == null
      && replica.cloud_head_seq == null) return "Replica freshness unknown";
    const sync = replica.last_synced_at == null ? "Last synchronized unknown" : `Last synchronized ${formatTime(replica.last_synced_at)}`;
    const gapCount = Number.isFinite(replica.gap_count) ? Math.max(0, Math.trunc(replica.gap_count)) : null;
    const gap = gapCount == null ? "" : gapCount > 0 ? ` · ${gapCount} gap${gapCount === 1 ? "" : "s"}` : " · no known gaps";
    const degraded = replica.journal_degraded ? " · journal degraded" : "";
    return `${sync}${gap}${degraded}`;
  }

  function renderContext(envelope) {
    const hostId = fence.value.hostId;
    const sessionId = fence.value.sessionId;
    const detail = byId("context-detail");
    detail.hidden = true;
    if (!sessionId) {
      setComponentState(byId("context-status"), "Select a session to inspect context.", "quiet");
      setText(byId("context-authority"), "—");
      return;
    }
    if (!envelope || !envelope.data) {
      setComponentState(byId("context-status"), componentMessage(envelope, "Context"), "error");
      setText(byId("context-authority"), "Unavailable");
      return;
    }
    const data = envelope.data;
    if (data.host_id !== hostId || data.session_id !== sessionId) {
      setComponentState(byId("context-status"), "Context data did not match the current selection.", "error");
      setText(byId("context-authority"), "Unavailable");
      return;
    }
    const resolve = data.context_resolve;
    const status = data.context_status;
    const replicaComponent = data.replica;
    const replica = replicaComponent?.data
      ? { ...replicaComponent.data, status: replicaComponent.status }
      : replicaComponent;
    const host = selectedHost();
    const hostLive = hostAvailability(host) === "online";
    const resolveAvailable = isContextResolverCurrent(envelope, resolve, hostAvailability(host));
    const contextResolveData = resolveAvailable ? resolve.data : null;
    const contextStatusData = envelope.status === "confirmed" && status?.status === "confirmed" && status?.data && hostLive ? status.data : null;
    const currentSummary = contextResolveData?.current_summary ?? {};
    const liveFreshness = contextResolveData?.freshness ?? {};

    setText(byId("context-authority"), resolveAvailable ? "Host live" : (replica?.status === "confirmed" ? "Fabric replica" : "Unavailable"));
    byId("context-authority").className = `authority-pill${resolveAvailable ? " authority-pill--live" : replica?.status === "unavailable" ? " authority-pill--unavailable" : ""}`;

    let statusMessage;
    let statusKind = "quiet";
    if (!hostLive) {
      statusMessage = `Current host context is unavailable · live route ${hostAvailability(host)}.`;
      statusKind = "stale";
    } else if (envelope.status !== "confirmed") {
      statusMessage = "Context snapshot is stale · current host state has not been reconfirmed.";
      statusKind = "stale";
    } else if (resolveAvailable) {
      statusMessage = contextResolveData.partial?.journal_degraded
        ? "Current host context confirmed · local journal reports degraded data."
        : componentMessage(resolve, "Current context");
      statusKind = contextResolveData.partial?.journal_degraded || resolve.status === "stale" ? "stale" : "quiet";
    } else if (resolve?.status === "unavailable") {
      statusMessage = `Current host context unavailable${resolve.error_code ? ` · ${boundedText(String(resolve.error_code), 64)}` : ""}.`;
      statusKind = "error";
    } else {
      statusMessage = "Current host context has not been confirmed.";
      statusKind = "stale";
    }
    if (envelope.status === "stale" && envelope.error_code) {
      statusMessage += ` Last refresh failed · ${boundedText(String(envelope.error_code), 64)}.`;
      statusKind = "stale";
    }
    setComponentState(byId("context-status"), statusMessage, statusKind);

    const freshness = byId("freshness-card");
    freshness.replaceChildren();
    const freshnessTop = createElement(documentRef, "div", "freshness-top");
    appendText(documentRef, freshnessTop, "strong", "", replicaFreshnessText(replica));
    const freshnessLabel = replica?.journal_degraded || (Number(replica?.gap_count) > 0) ? "Needs attention" : replica?.status === "confirmed" ? "Replica" : "Unknown";
    appendText(documentRef, freshnessTop, "span", "state-badge state-badge--muted", freshnessLabel);
    freshness.append(freshnessTop);
    const replicaDetails = createElement(documentRef, "div", "freshness-detail");
    const revision = replica?.source_head_revision ?? replica?.acked_through_revision ?? replica?.cloud_head_seq;
    const replicaFacts = [
      ["Source revision", revision == null ? "Unknown" : boundedText(String(revision), 48)],
      ["Journal", replica?.journal_degraded ? "Degraded" : replica?.status === "confirmed" ? "Healthy" : "Unknown"],
      ["Current journal revision", liveFreshness.resolved_revision ?? contextStatusData?.journal?.revision ?? "Unavailable"],
      ["Memory worker", contextResolveData?.memory?.worker ?? contextStatusData?.memory?.worker ?? "Unavailable"],
    ];
    if (contextResolveData?.current_summary) {
      const summary = contextResolveData.current_summary;
      replicaFacts.push(["Tasks active / total", `${summary.tasks_active ?? "?"} / ${summary.tasks_total ?? "?"}`]);
      replicaFacts.push(["Needs attention", summary.tasks_attention ?? "?"]);
      replicaFacts.push(["Observations", summary.observations ?? "?"]);
      replicaFacts.push(["Last observed", formatTime(summary.last_observed_at)]);
    } else if (contextStatusData?.journal) {
      replicaFacts.push(["Journal observations", contextStatusData.journal.observations ?? "?"]);
      replicaFacts.push(["Journal revision", contextStatusData.journal.revision ?? "?"]);
    }
    for (const [label, value] of replicaFacts) {
      const fact = createElement(documentRef, "div", "");
      appendText(documentRef, fact, "span", "", label);
      appendText(documentRef, fact, "strong", "", value);
      replicaDetails.append(fact);
    }
    freshness.append(replicaDetails);

    const unresolvedRoot = byId("unresolved-list");
    unresolvedRoot.replaceChildren();
    const unresolved = contextResolveData?.unresolved;
    if (!Array.isArray(unresolved)) {
      appendText(documentRef, unresolvedRoot, "div", "compact-item", resolve?.status === "unavailable" ? "Unresolved items unavailable." : "Current unresolved items are not confirmed.");
      setText(byId("unresolved-count"), "?");
    } else if (unresolved.length === 0) {
      appendText(documentRef, unresolvedRoot, "div", "compact-item", "No unresolved items in the confirmed projection.");
      setText(byId("unresolved-count"), "0");
    } else {
      setText(byId("unresolved-count"), String(unresolved.length));
      for (const item of unresolved.slice(0, 16)) {
        const row = createElement(documentRef, "div", "compact-item");
        const statusName = boundedText(String(item?.status ?? "unknown"), 32);
        const taskName = shortIdentifier(item?.task_id, 96);
        appendText(documentRef, row, "strong", "", `${taskName || "Task"} · ${statusName}`);
        if (item?.reason) appendText(documentRef, row, "span", "compact-item-meta", boundedText(String(item.reason), 180));
        unresolvedRoot.append(row);
      }
    }

    const rollupRoot = byId("rollup-list");
    rollupRoot.replaceChildren();
    const rollups = contextResolveData?.recent_related_tasks;
    if (!Array.isArray(rollups)) {
      appendText(documentRef, rollupRoot, "div", "compact-item", "Recent context rollups unavailable.");
    } else if (rollups.length === 0) {
      appendText(documentRef, rollupRoot, "div", "compact-item", "No recent task rollups in this projection.");
    } else {
      for (const task of rollups.slice(0, 8)) {
        const row = createElement(documentRef, "div", "compact-item");
        const taskId = shortIdentifier(task?.task_id, 96) || "Task";
        const backend = boundedText(String(task?.backend ?? "unknown"), 48);
        const taskStatus = boundedText(String(task?.state?.status ?? "unobserved"), 32);
        appendText(documentRef, row, "strong", "", `${taskId} · ${taskStatus}`);
        appendText(documentRef, row, "span", "compact-item-meta", `${backend} · observed ${formatTime(task?.last_observed_at)}`);
        rollupRoot.append(row);
      }
    }

    const provenanceBlock = byId("provenance-block");
    provenanceBlock.replaceChildren();
    const refs = Array.isArray(contextResolveData?.refs) ? contextResolveData.refs.slice(0, 8) : [];
    provenanceBlock.hidden = refs.length === 0;
    if (refs.length > 0) {
      appendText(documentRef, provenanceBlock, "h3", "", "Provenance references");
      const chips = createElement(documentRef, "div", "provenance-list");
      for (const reference of refs) {
        const id = shortIdentifier(reference?.observation_id, 64);
        const kind = boundedText(String(reference?.kind ?? "reference"), 32);
        const revisionText = reference?.revision == null ? "" : ` · r${boundedText(String(reference.revision), 24)}`;
        const chip = appendText(documentRef, chips, "span", "provenance-chip", `${kind}${id ? ` · ${id}` : ""}${revisionText}`);
        chip.title = chip.textContent;
      }
      provenanceBlock.append(chips);
    }
    detail.hidden = false;
  }

  function renderTimeline(envelope) {
    const root = byId("timeline-list");
    root.replaceChildren();
    const sessionId = fence.value.sessionId;
    if (!sessionId) {
      setComponentState(byId("timeline-status"), "Select a session to load recent observations.", "quiet");
      return;
    }
    const data = envelope?.data;
    if (!envelope || !data || !Array.isArray(data.events)) {
      setComponentState(byId("timeline-status"), componentMessage(envelope, "Timeline"), "error");
      return;
    }
    if (data.host_id !== fence.value.hostId || data.session_id !== sessionId) {
      setComponentState(byId("timeline-status"), "Timeline data did not match the current selection.", "error");
      return;
    }
    const source = data.source ?? {};
    setText(byId("timeline-authority"), envelope.authority === "fabric_replica" ? "Fabric replica" : "Replica");
    setComponentState(
      byId("timeline-status"),
      `${componentMessage(envelope, "Sanitized timeline")} · ${replicaFreshnessText(source)}`,
      envelope.status === "stale" || source.journal_degraded || Number(source.gap_count) > 0 ? "stale" : "quiet",
    );
    const events = state.timelineEvents.length > 0 ? state.timelineEvents : data.events;
    if (events.length === 0) {
      appendText(documentRef, root, "li", "component-state component-state--quiet", "No replicated observations in this window.");
      return;
    }
    for (const event of events.slice(-MAX_TIMELINE_EVENTS)) {
      const item = createElement(documentRef, "li", "timeline-item");
      appendText(documentRef, item, "span", "timeline-marker", "").setAttribute("aria-hidden", "true");
      const parts = [event?.kind, event?.action].filter((part) => typeof part === "string" && part.length > 0).map((part) => boundedText(part, 48));
      const title = parts.length > 0 ? parts.join(" · ") : "Sanitized observation";
      appendText(documentRef, item, "strong", "timeline-title", title);
      const metadata = [
        event?.target_backend && `Backend ${boundedText(String(event.target_backend), 40)}`,
        event?.content_kind && `Content ${boundedText(String(event.content_kind), 32)}`,
        event?.state_status && `State ${boundedText(String(event.state_status), 32)}`,
      ].filter(Boolean);
      if (metadata.length > 0) appendText(documentRef, item, "span", "timeline-detail", metadata.join(" · "));
      appendText(documentRef, item, "span", "timeline-time", formatTime(event?.observed_at));
      const revision = event?.source_revision ?? event?.state_revision ?? event?.cloud_seq;
      if (revision != null) appendText(documentRef, item, "span", "timeline-source", `Revision ${boundedText(String(revision), 32)}`);
      root.append(item);
    }
  }

  function scheduleNextRefresh() {
    clearTimeout(state.timer);
    const delay = documentRef.hidden ? REFRESH_BACKGROUND_MS : REFRESH_FOREGROUND_MS;
    state.timer = setTimeout(() => void refreshDashboard(false), delay);
  }

  function currentRefreshLabel() {
    if (!state.lastUpdatedAt) return "Connecting";
    const seconds = Math.max(0, Math.floor((Date.now() - state.lastUpdatedAt) / 1_000));
    if (state.degraded.size > 0) return `Degraded · updated ${seconds}s ago`;
    return `Live · updated ${seconds}s ago`;
  }

  async function refreshDashboard(manual = false) {
    if (state.refreshing) return;
    state.refreshing = true;
    byId("refresh-button").disabled = true;
    setRefreshStatus(manual ? "Refreshing" : currentRefreshLabel(), manual ? "warning" : state.degraded.size ? "warning" : "live");
    try {
      await Promise.all([loadBootstrap(), loadHosts()]);
      const snapshot = fence.snapshot();
      if (snapshot.hostId) {
        if (membershipRemovalConfirmed(state.hostsEnvelope, snapshot.hostId)) {
          await loadSessions(snapshot);
        } else if (snapshot.sessionId) {
          await Promise.all([loadSessions(snapshot), loadSessionDetails(snapshot)]);
        } else {
          await loadSessions(snapshot);
        }
      }
      state.lastUpdatedAt = Date.now();
      setText(byId("last-updated"), `Updated ${formatTime(state.lastUpdatedAt)}`);
      setRefreshStatus(currentRefreshLabel(), state.degraded.size > 0 ? "warning" : "live");
    } catch {
      setRefreshStatus("Refresh failed · showing available components", "error");
    } finally {
      state.refreshing = false;
      byId("refresh-button").disabled = false;
      scheduleNextRefresh();
    }
  }

  function onLocationChange() {
    const next = parseSelection(windowRef.location);
    select(next, false);
  }

  function onVisibilityChange() {
    clearTimeout(state.timer);
    renderHosts();
    renderSession(state.sessionEnvelope);
    renderTasks(state.tasksEnvelope);
    renderContext(state.contextEnvelope);
    renderTimeline(state.timelineEnvelope);
    if (documentRef.hidden) scheduleNextRefresh();
    else void refreshDashboard(false);
  }

  function markLiveRoutesStale() {
    state.sessionsEnvelope = markEnvelopeStale(state.sessionsEnvelope, "browser_offline");
    state.sessionEnvelope = markEnvelopeStale(state.sessionEnvelope, "browser_offline");
    state.tasksEnvelope = markEnvelopeStale(state.tasksEnvelope, "browser_offline");
    state.contextEnvelope = markEnvelopeStale(state.contextEnvelope, "browser_offline");
  }

  byId("refresh-button").addEventListener("click", () => void refreshDashboard(true));
  windowRef.addEventListener("hashchange", onLocationChange);
  documentRef.addEventListener("visibilitychange", onVisibilityChange);
  windowRef.addEventListener("online", () => {
    state.browserOffline = false;
    renderHosts();
    renderSession(state.sessionEnvelope);
    renderTasks(state.tasksEnvelope);
    renderContext(state.contextEnvelope);
    renderTimeline(state.timelineEnvelope);
    void refreshDashboard(false);
  });
  windowRef.addEventListener("offline", () => {
    state.browserOffline = true;
    markLiveRoutesStale();
    renderHosts();
    renderSession(state.sessionEnvelope);
    renderTasks(state.tasksEnvelope);
    renderContext(state.contextEnvelope);
    renderTimeline(state.timelineEnvelope);
    setRefreshStatus("Browser offline · live routes unknown", "warning");
    scheduleNextRefresh();
  });

  renderHosts();
  renderSession(null);
  renderTasks(null);
  renderContext(null);
  renderTimeline(null);
  void refreshDashboard(false);
  return { state, fence, refreshDashboard };
}

let activeDashboard = null;
if (typeof document !== "undefined" && typeof window !== "undefined") {
  activeDashboard = startDashboard(document, window);
}

export {
  REFRESH_BACKGROUND_MS,
  REFRESH_FOREGROUND_MS,
  SelectionFence,
  RequestCoordinator,
  activeDashboard,
  boundedText,
  compareSequence,
  formatTime,
  mergePendingSummary,
  mergeTaskProjection,
  parseSelection,
  parseTimestampMs,
  pendingForDisplay,
  isContextResolverCurrent,
  effectiveHostAvailability,
  membershipRemovalConfirmed,
  nextPendingExpiryMs,
  isSessionProjectionCurrent,
  isTaskProjectionCurrent,
  markEnvelopeStale,
  responseIsDegraded,
  retainLastData,
  setText,
  taskBackendPresentation,
  shortIdentifier,
};
