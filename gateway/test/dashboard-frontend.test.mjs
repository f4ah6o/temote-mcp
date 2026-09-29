import test from "node:test";
import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import {
  RequestCoordinator,
  SelectionFence,
  compareSequence,
  effectiveHostAvailability,
  formatTime,
  isConfirmedCompleteSessionMiss,
  isContextComponentCurrent,
  isContextResolverCurrent,
  isSessionProjectionCurrent,
  isTaskProjectionCurrent,
  markEnvelopeStale,
  membershipRemovalConfirmed,
  mergePendingSummary,
  mergeTaskProjection,
  nextPendingExpiryMs,
  parseSelection,
  pendingForDisplay,
  retainLastData,
  responseIsDegraded,
  setText,
  taskBackendPresentation,
  unresolvedPresentation,
} from "../assets/dash/app.js";
import { handle as handleDashboardFixture, state as dashboardFixtureState } from "./dashboard-fixtures/server.mjs";

const futureObservedAt = 1_800_000_000;
const futureExpiresAt = futureObservedAt + 30;

function summary(state, overrides = {}) {
  return {
    state,
    summary_revision: "4",
    observed_at: futureObservedAt,
    producer_kind: "runtime_owner",
    producer_epoch: "9",
    expires_at: futureExpiresAt,
    ...(state === "pending" ? { count: 1, types: ["approval"] } : {}),
    ...overrides,
  };
}

function task(revision, pending, overrides = {}) {
  return {
    backend: "codex",
    task_id: "task-1",
    status: "running",
    revision,
    last_updated_at: futureObservedAt,
    pending_interaction: pending,
    ...overrides,
  };
}

test("selection comes from query/hash without using browser history and identifiers are bounded", () => {
  assert.deepEqual(parseSelection({
    search: "?host=query-host&session=query-session",
    hash: "#host=hash-host&session=hash-session",
  }), { hostId: "hash-host", sessionId: "hash-session" });
  assert.deepEqual(parseSelection({ search: "?host=query-host", hash: "" }), {
    hostId: "query-host",
    sessionId: "",
  });
  assert.equal(parseSelection({ search: `?host=${"x".repeat(160)}`, hash: "" }).hostId.length, 128);
});

test("a capped host session list does not contradict a successful selected-session lookup", () => {
  const capped = {
    status: "stale",
    data: { truncated: true, sessions: Array.from({ length: 256 }, (_, index) => ({ session_id: `session-${index}` })) },
  };
  const successfulDetail = {
    status: "confirmed",
    data: { session_id: "session-299", session: { status: "active" } },
  };
  assert.equal(successfulDetail.status, "confirmed");
  assert.equal(isConfirmedCompleteSessionMiss(capped, "session-299"), false);
  assert.equal(isConfirmedCompleteSessionMiss({ status: "confirmed", data: { truncated: true, sessions: capped.data.sessions } }, "session-299"), false);
  assert.equal(isConfirmedCompleteSessionMiss({ status: "confirmed", data: { sessions: [{ session_id: "session-1" }] } }, "session-299"), true);
  assert.equal(isConfirmedCompleteSessionMiss({ status: "stale", data: { sessions: [{ session_id: "session-1" }] } }, "session-299"), false);
});

test("a selection fence prevents an older selection response from applying", () => {
  const fence = new SelectionFence("host-a", "session-old");
  const oldRequest = fence.snapshot();
  const newRequest = fence.select("host-a", "session-new");
  assert.equal(fence.matches(oldRequest), false);
  assert.equal(fence.matches(newRequest), true);
});

test("same-key fetches are deduplicated and use a fixed same-origin GET request", async () => {
  let calls = 0;
  let resolveFetch;
  let receivedOptions;
  const coordinator = new RequestCoordinator((path, options) => {
    calls += 1;
    assert.equal(path, "/dash/api/v1/hosts");
    receivedOptions = options;
    return new Promise((resolve) => { resolveFetch = resolve; });
  });
  const first = coordinator.request("/dash/api/v1/hosts", "hosts");
  const second = coordinator.request("/dash/api/v1/hosts", "hosts");
  assert.equal(calls, 1);
  assert.equal(receivedOptions.method, "GET");
  assert.equal(receivedOptions.credentials, "same-origin");
  assert.equal(receivedOptions.cache, "no-store");
  assert.equal(receivedOptions.headers.accept, "application/json");
  resolveFetch({ ok: true, json: async () => ({ status: "confirmed", authority: "fabric", freshness: "current", data: { hosts: [] } }) });
  const [firstResult, secondResult] = await Promise.all([first, second]);
  assert.equal(firstResult.status, "confirmed");
  assert.equal(secondResult.status, "confirmed");
});

test("loopback fixture can switch from a live host to unknown and partial backend data", async () => {
  const saved = { ...dashboardFixtureState };
  try {
    Object.assign(dashboardFixtureState, {
      hostsUnavailable: false,
      membershipMissing: false,
      replicaUnavailable: false,
      livenessUnavailable: false,
      hostOffline: false,
      tasksUnavailable: false,
      codexUnavailable: false,
      tasksSkipped: 0,
      tasksTruncated: false,
    });
    const normalResponse = await handleDashboardFixture(new Request("http://fixture/dash/api/v1/hosts"));
    const normal = await normalResponse.json();
    assert.equal(normalResponse.status, 200);
    assert.equal(normal.data.hosts[0].availability, "online");

    dashboardFixtureState.livenessUnavailable = true;
    const unknownResponse = await handleDashboardFixture(new Request("http://fixture/dash/api/v1/hosts"));
    const unknown = await unknownResponse.json();
    assert.equal(unknown.data.hosts[0].availability, "unknown");
    assert.equal(unknown.data.components.liveness.status, "unavailable");

    dashboardFixtureState.livenessUnavailable = false;
    dashboardFixtureState.tasksSkipped = 2;
    dashboardFixtureState.tasksTruncated = true;
    const partialResponse = await handleDashboardFixture(new Request("http://fixture/dash/api/v1/hosts/fabric-local/sessions/session-demo-01/tasks"));
    const partial = await partialResponse.json();
    assert.equal(partial.status, "stale");
    assert.equal(partial.data.backends[0].status, "confirmed");
    assert.equal(partial.data.backends[0].skipped, 2);
    assert.equal(partial.data.backends[0].truncated, true);

    dashboardFixtureState.membershipMissing = true;
    dashboardFixtureState.replicaUnavailable = true;
    const removedResponse = await handleDashboardFixture(new Request("http://fixture/dash/api/v1/hosts"));
    const removed = await removedResponse.json();
    assert.equal(removed.status, "stale");
    assert.equal(removed.data.components.membership.status, "confirmed");
    assert.equal(removed.data.components.replica.status, "unavailable");
    assert.equal(membershipRemovalConfirmed(removed, "fabric-local"), true);
  } finally {
    Object.assign(dashboardFixtureState, saved);
  }
});

test("changing selection aborts old scoped requests", async () => {
  let signal;
  const coordinator = new RequestCoordinator((_path, options) => {
    signal = options.signal;
    return new Promise((_resolve, reject) => {
      signal.addEventListener("abort", () => reject(new Error("aborted")), { once: true });
    });
  });
  const oldRequest = coordinator.request("/dash/api/v1/hosts/a/sessions/1/tasks", "tasks", 3);
  coordinator.abortSelectionExcept(4);
  const result = await oldRequest;
  assert.equal(signal.aborted, true);
  assert.equal(result.status, "unavailable");
  assert.equal(result.error_code, "request_cancelled");
});

test("task revision and pending summary revision advance independently", () => {
  const original = task(12, summary("none"));
  const changedSummary = task(12, summary("pending", {
    summary_revision: "5",
    count: 2,
    types: ["approval", "question"],
  }), { status: "running" });
  const merged = mergeTaskProjection(original, changedSummary);
  assert.equal(merged.revision, 12);
  assert.equal(merged.status, "running");
  assert.equal(merged.pending_interaction.state, "pending");
  assert.equal(merged.pending_interaction.summary_revision, "5");
  assert.deepEqual(merged.pending_interaction.types, ["approval", "question"]);
});

test("out-of-order task revisions cannot roll back task state or downgrade supported metadata", () => {
  const current = task(20, summary("none", { summary_revision: "8" }));
  const lateOldTask = task(19, { state: "unsupported" }, { status: "starting" });
  const merged = mergeTaskProjection(current, lateOldTask);
  assert.equal(merged.status, "running");
  assert.equal(merged.revision, 20);
  assert.equal(merged.pending_interaction.state, "none");
});

test("a supported summary replaces unsupported metadata once observed", () => {
  const merged = mergeTaskProjection(
    task(7, { state: "unsupported" }),
    task(7, summary("none", { summary_revision: "0" })),
  );
  assert.equal(merged.pending_interaction.state, "none");
  assert.equal(merged.pending_interaction.summary_revision, "0");
});

test("producer epochs are compared separately from summary revision", () => {
  const previous = summary("pending", { producer_epoch: "10", summary_revision: "18" });
  const delayedOldProducer = summary("none", { producer_epoch: "9", summary_revision: "99" });
  assert.equal(mergePendingSummary(previous, delayedOldProducer).state, "pending");

  const replacementProducer = summary("none", { producer_epoch: "11", summary_revision: "0" });
  const replaced = mergePendingSummary(previous, replacementProducer);
  assert.equal(replaced.state, "none");
  assert.equal(replaced.producer_epoch, "11");
  assert.equal(replaced.summary_revision, "0");
});

test("producer kind changes do not compare unrelated epochs", () => {
  const runtimeOwner = summary("pending", { producer_epoch: "40", producer_kind: "runtime_owner" });
  const remoteObserver = summary("none", { producer_epoch: "1", producer_kind: "host_remote_observer" });
  assert.equal(mergePendingSummary(runtimeOwner, remoteObserver).state, "pending");
  assert.equal(mergePendingSummary(runtimeOwner, { ...remoteObserver, summary_revision: "5" }, 2).producer_kind, "host_remote_observer");
});

test("an expired unavailable projection wins at the same revision and rejects delayed fresh-looking none", () => {
  const priorNone = summary("none", { count: undefined, types: undefined });
  const serverExpiry = {
    state: "unavailable",
    summary_revision: priorNone.summary_revision,
    observed_at: priorNone.observed_at,
    producer_kind: priorNone.producer_kind,
    producer_epoch: priorNone.producer_epoch,
    expires_at: priorNone.expires_at,
  };
  const expired = mergePendingSummary(priorNone, serverExpiry);
  assert.equal(expired.state, "unavailable");
  const delayedNone = { ...priorNone };
  assert.equal(mergePendingSummary(expired, delayedNone).state, "unavailable");
});

test("u64 revisions compare exactly as decimal strings and unsafe numbers fail closed", () => {
  assert.equal(compareSequence("9007199254740993", "9007199254740992"), 1);
  assert.equal(compareSequence("18446744073709551615", "18446744073709551614"), 1);
  assert.equal(compareSequence("18446744073709551616", "1"), null);
  assert.equal(compareSequence(9_007_199_254_740_992, 9_007_199_254_740_991), null);
  assert.equal(mergeTaskProjection(task(9_007_199_254_740_992, summary("none")), task(9_007_199_254_740_991, summary("pending"))).pending_interaction.state, "none");
});

test("pending summaries expire at the TTL boundary and missing expiry is unavailable", () => {
  const now = futureObservedAt * 1_000;
  assert.equal(pendingForDisplay(summary("none"), now).display_state, "none");
  assert.equal(pendingForDisplay(summary("none"), futureExpiresAt * 1_000).display_state, "unavailable");
  assert.equal(pendingForDisplay({ state: "pending" }, now).display_state, "unavailable");
});

test("local expiry scheduling chooses the nearest supported summary deadline without fetching", () => {
  const now = futureObservedAt * 1_000;
  const tasks = [
    task(1, summary("pending", { expires_at: futureExpiresAt * 1_000 })),
    task(2, summary("none", { expires_at: (futureExpiresAt + 5) * 1_000 })),
    task(3, { state: "unsupported" }),
  ];
  assert.equal(nextPendingExpiryMs(tasks, now), futureExpiresAt * 1_000);
  assert.equal(nextPendingExpiryMs(tasks, (futureExpiresAt + 1) * 1_000), (futureExpiresAt + 5) * 1_000);
  assert.equal(nextPendingExpiryMs([{ pending_interaction: { state: "unsupported" } }], now), null);
});

test("component failures retain only visibly stale data and stale projections are never current", () => {
  const prior = { status: "confirmed", authority: "host_live", freshness: "live", data: { current_summary: { tasks_total: 2 } } };
  const unavailable = { status: "unavailable", authority: "unavailable", freshness: "unavailable", error_code: "host_offline" };
  const stale = retainLastData(prior, unavailable);
  assert.equal(stale.status, "stale");
  assert.equal(stale.data, prior.data);
  assert.equal(isSessionProjectionCurrent(stale, "online"), false);
  assert.equal(isTaskProjectionCurrent("stale", "confirmed", "online"), false);
  assert.equal(isTaskProjectionCurrent("confirmed", "unavailable", "online"), false);
  const resolver = { status: "confirmed", authority: "host_live", freshness: "live", data: { freshness: { stale: false } } };
  assert.equal(isContextResolverCurrent(stale, resolver, "online"), false);
  assert.equal(isContextResolverCurrent(prior, resolver, "online"), true);
  assert.equal(isContextResolverCurrent(prior, { ...resolver, authority: "fabric_replica" }, "online"), false);
});

test("a fresh live context resolver remains visible through unrelated outer partial failure", () => {
  const outerPartial = {
    status: "stale",
    authority: "fabric",
    freshness: "stale",
    data: {},
  };
  const liveResolver = {
    status: "confirmed",
    authority: "host_live",
    freshness: "live",
    data: { freshness: { resolved_revision: "42", stale: false } },
  };
  const liveStatus = {
    status: "confirmed",
    authority: "host_live",
    freshness: "live",
    data: { journal: { revision: "42" } },
  };
  assert.equal(isContextResolverCurrent(outerPartial, liveResolver, "online"), true);
  assert.equal(isContextComponentCurrent(outerPartial, liveStatus, "online"), true);
  assert.equal(isContextResolverCurrent(outerPartial, {
    ...liveResolver,
    data: { freshness: { stale: true } },
  }, "online"), false);
  const failedCachedPoll = { ...outerPartial, error_code: "context_refresh_failed", stale_error: true };
  assert.equal(isContextResolverCurrent(failedCachedPoll, liveResolver, "online"), false);
  assert.equal(isContextComponentCurrent(failedCachedPoll, liveStatus, "online"), false);
  assert.equal(isContextResolverCurrent(outerPartial, liveResolver, "unknown"), false);
});

test("offline transitions mark retained live routes stale until a fresh response arrives", () => {
  const live = { status: "confirmed", authority: "host_live", freshness: "live", data: { host_id: "host-a" } };
  const stale = markEnvelopeStale(live, "browser_offline");
  assert.equal(stale.status, "stale");
  assert.equal(stale.stale_reason, "browser_offline");
  assert.equal(stale.data, live.data);
  assert.equal(isSessionProjectionCurrent(stale, "online"), false);
  assert.equal(markEnvelopeStale({ status: "unavailable", error_code: "x" }).status, "unavailable");
});

test("failed host inventory or liveness cannot leave a cached host marked live", () => {
  const host = { host_id: "host-a", availability: "online" };
  assert.equal(effectiveHostAvailability(host, {
    status: "confirmed",
    data: { hosts: [host], components: { liveness: { status: "confirmed" } } },
  }), "online");
  assert.equal(effectiveHostAvailability(host, {
    status: "unavailable",
    data: undefined,
  }), "unknown");
  assert.equal(effectiveHostAvailability(host, {
    status: "stale",
    data: { hosts: [host], components: { liveness: { status: "unavailable" } } },
  }), "unknown");
  assert.equal(effectiveHostAvailability(host, {
    status: "confirmed",
    data: { hosts: [host], components: { liveness: { status: "confirmed" } } },
  }, true), "unknown");
  assert.equal(isSessionProjectionCurrent({ status: "confirmed" }, effectiveHostAvailability(host, {
    status: "confirmed",
    data: { hosts: [host], components: { liveness: { status: "confirmed" } } },
  }, true)), false);
});

test("selected host data is cleared only after confirmed membership removal", () => {
  const removed = {
    status: "confirmed",
    data: {
      components: { membership: { status: "confirmed" } },
      hosts: [{ host_id: "host-b" }],
    },
  };
  assert.equal(membershipRemovalConfirmed(removed, "host-a"), true);
  assert.equal(membershipRemovalConfirmed(removed, "host-b"), false);
  assert.equal(membershipRemovalConfirmed({ ...removed, data: { ...removed.data, components: { membership: { status: "stale" } } } }, "host-a"), false);
  assert.equal(membershipRemovalConfirmed({ ...removed, data: { ...removed.data, components: { membership: { status: "unavailable" } } } }, "host-a"), false);
});

test("partial child failures and truncated lists degrade the header, replica staleness alone does not", () => {
  assert.equal(responseIsDegraded({
    status: "stale",
    data: { backends: [{ backend: "codex", status: "unavailable", error_code: "task_store_unavailable" }] },
  }), true);
  assert.equal(responseIsDegraded({
    status: "stale",
    data: { backends: [{ backend: "codex", status: "confirmed", tasks: [], skipped: 2 }] },
  }), true);
  assert.equal(responseIsDegraded({
    status: "confirmed",
    data: { replica: { status: "stale", freshness: "stale", authority: "fabric_replica" } },
  }), false);
});

test("partial backend rows remain visible but cannot claim a current task snapshot", () => {
  const presentation = taskBackendPresentation("stale", {
    status: "confirmed",
    tasks: [{ task_id: "task-1" }],
    total: 3,
    skipped: 2,
    truncated: true,
  }, "online");
  assert.equal(presentation.available, true);
  assert.equal(presentation.current, false);
  assert.equal(presentation.partial, true);
  assert.equal(presentation.skipped, 2);
  assert.equal(presentation.truncated, true);
  assert.equal(presentation.tasks.length, 1);

  const complete = taskBackendPresentation("confirmed", { status: "confirmed", tasks: [], total: 0, skipped: 0, truncated: false }, "online");
  assert.equal(complete.available, true);
  assert.equal(complete.current, true);
  assert.equal(complete.partial, false);
});

test("unresolved items never claim an empty confirmed projection when unavailable", () => {
  for (const bad of [undefined, null, "broken", {}, 0]) {
    const presentation = unresolvedPresentation("confirmed", bad);
    assert.equal(presentation.state, "unavailable");
    assert.equal(presentation.message, "Current unresolved items are not confirmed.");
    assert.equal(presentation.message.includes("No unresolved items"), false);
  }
  const unavailable = unresolvedPresentation("unavailable", undefined);
  assert.equal(unavailable.state, "unavailable");
  assert.equal(unavailable.message, "Unresolved items unavailable.");
  const empty = unresolvedPresentation("confirmed", []);
  assert.equal(empty.state, "empty");
  assert.equal(empty.message, "No unresolved items in the confirmed projection.");
  const list = unresolvedPresentation("confirmed", [{ task_id: "task-1" }]);
  assert.equal(list.state, "list");
  assert.equal(list.count, "1");
});

test("malformed and out-of-range timestamps render as unknown", () => {
  assert.equal(formatTime("18446744073709551615"), "Unknown");
  assert.equal(formatTime(18_446_744_073_709_551_615), "Unknown");
  assert.equal(formatTime("not-a-date"), "Unknown");
});

test("untrusted values go through textContent and dashboard assets avoid HTML injection sinks", async () => {
  const hostile = `</span><img src=x onerror="globalThis.pwned=true">`;
  const element = { textContent: "", title: "" };
  setText(element, hostile);
  assert.equal(element.textContent, hostile);
  assert.equal(element.title, hostile);
  const app = await readFile(new URL("../assets/dash/app.js", import.meta.url), "utf8");
  const html = await readFile(new URL("../assets/dash/index.html", import.meta.url), "utf8");
  assert.doesNotMatch(app, /\.innerHTML\b|\.outerHTML\b|insertAdjacentHTML|\beval\s*\(/);
  assert.doesNotMatch(html, /https?:\/\//);
  assert.match(html, /<script type="module" src="\/dash\/app\.js"><\/script>/);
});
