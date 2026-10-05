// Keeps transport and selection state independent of the host SDK and DOM.
export class FabricController {
  constructor(call, changed) {
    this.call = call;
    this.changed = changed;
    this.state = { hostId: "", sessionId: "", overview: null, sessions: null, detail: null, busy: false, error: "" };
    this.epoch = 0;
    this.inFlight = new Map();
    this.refreshes = new Map();
  }

  initial(result) {
    if (!this.state.overview && result?.structuredContent?.kind === "fabric_overview") {
      this.state.overview = result.structuredContent;
      this.changed(this.state);
    }
  }

  select(hostId, sessionId = "") {
    if (hostId === this.state.hostId && sessionId === this.state.sessionId) return;
    const sameHost = hostId === this.state.hostId;
    this.epoch += 1;
    this.state = { ...this.state, hostId, sessionId, detail: null, busy: false, error: "",
      sessions: sameHost ? this.state.sessions : null };
    this.changed(this.state);
    return this.refresh();
  }

  request(name, args) {
    const key = JSON.stringify([name, args]);
    if (this.inFlight.has(key)) return this.inFlight.get(key);
    const promise = Promise.resolve().then(() => this.call({ name, arguments: args })).then((result) => {
      const view = result?.structuredContent;
      if (result?.isError || !view || view.kind !== name) throw new Error("fabric_read_unavailable");
      const scoped = name === "fabric_session_list" ? view.sessions : view.session;
      if (name !== "fabric_overview"
        && (scoped?.data?.host_id !== args.host_id
          || (name === "fabric_session_read" && scoped?.data?.session_id !== args.session_id))) {
        throw new Error("fabric_scope_mismatch");
      }
      return view;
    }).finally(() => this.inFlight.delete(key));
    this.inFlight.set(key, promise);
    return promise;
  }

  refresh() {
    const epoch = this.epoch;
    if (this.refreshes.has(epoch)) return this.refreshes.get(epoch);
    const { hostId, sessionId } = this.state;
    this.state.busy = true;
    this.state.error = "";
    this.changed(this.state);
    const reads = [["overview", "fabric_overview", {}]];
    if (hostId) reads.push(["sessions", "fabric_session_list", { host_id: hostId }]);
    if (hostId && sessionId) reads.push(["detail", "fabric_session_read", { host_id: hostId, session_id: sessionId }]);
    const promise = Promise.all(reads.map(async ([key, name, args]) => {
      try {
        const next = await this.request(name, args);
        if (epoch !== this.epoch) return;
        this.state[key] = retainView(this.state[key], next);
      } catch {
        if (epoch !== this.epoch) return;
        this.state[key] = staleView(this.state[key]);
        this.state.error = "Some state could not be read. Refresh to retry.";
      }
    })).then(() => {
      if (epoch === this.epoch) {
        this.state.busy = false;
        this.changed(this.state);
      }
    }).finally(() => this.refreshes.delete(epoch));
    this.refreshes.set(epoch, promise);
    return promise;
  }
}

export function retainView(previous, next) {
  if (!previous) return next;
  const view = { ...next };
  for (const key of ["inventory", "sessions", "session", "tasks"]) {
    const envelope = next[key];
    if (envelope?.status === "unavailable" && envelope.error_code !== "session_not_found"
      && envelope.error_code !== "host_not_found" && previous[key]?.data) {
      view[key] = { ...envelope, data: previous[key].data, freshness: "stale", retained: true };
    }
  }
  // A failed selected-session read must invalidate retained task information too.
  if (next.session?.status === "unavailable" && !next.tasks && previous.tasks
    && !["session_not_found", "host_not_found"].includes(next.session.error_code)) {
    view.tasks = { ...previous.tasks, freshness: "stale", retained: true };
  }
  return view;
}

export function staleView(view) {
  if (!view) return null;
  const next = { ...view };
  for (const key of ["service", "inventory", "sessions", "session", "tasks"]) {
    if (view[key]) next[key] = { ...view[key], freshness: "stale", retained: true };
  }
  return next;
}

export function refreshDelay(hidden) {
  return hidden ? 30_000 : 5_000;
}
