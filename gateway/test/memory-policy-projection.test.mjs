import assert from "node:assert/strict";
import { createHash, randomUUID } from "node:crypto";
import test from "node:test";

import { loadMemoryConfiguration } from "../src/memory/config.js";
import {
  boundedInput,
  extractKnowledge,
  extractionInputBudget,
  MemoryError,
  toExtractorObservation,
} from "../src/memory/extractor.js";
import {
  explicitRepositoryPredecessors,
  permittedRepositoryClauses,
} from "../src/memory/safety.js";
import {
  MEMORY_TEST_HOST_ID,
  MEMORY_TEST_HOST_TOKEN,
  MEMORY_TEST_OWNER,
  startMemoryRuntime,
} from "./helpers/memory-runtime.mjs";

const PROVIDER_ENV = Object.freeze({
  MEMORY_ENABLED: "true",
  MEMORY_EXTRACTOR: "openai_compatible",
  MEMORY_ENDPOINT: "https://extractor.example/v1/chat/completions",
  MEMORY_MODEL: "policy-projection-test-model",
  MEMORY_API_KEY: "test-only-provider-key",
});
const POLICY = "Report output format must be JSON.";
const QUESTION = "The required report field set remains undecided.";
const REPOSITORY = "github:example/repository";
const FENCE = String.fromCharCode(96).repeat(3);

function observation({
  cloudSeq = 1,
  ownerId = "owner-a",
  repositoryKey = REPOSITORY,
  contentPreview = policyInstruction(),
  contentKind = "text",
  kind = "instruction",
  taskId = "task-a",
} = {}) {
  return {
    cloud_seq: cloudSeq,
    owner_id: ownerId,
    host_id: "host-a",
    session_id: "session-a",
    source_revision: cloudSeq,
    observation_id: "observation-" + cloudSeq,
    repository_key: repositoryKey,
    workspace_id: null,
    task_id: taskId,
    execution_id: null,
    operation_id: null,
    kind,
    action: "task_start",
    target_backend: "codex",
    content_kind: contentKind,
    content_preview: contentPreview,
    state_status: null,
    state_revision: null,
    evidence_refs: "[]",
    observed_at: "2026-09-28T00:00:00Z",
  };
}

function policyInstruction({
  quote = POLICY,
  changed = false,
  question = QUESTION,
  predecessor = null,
} = {}) {
  return [
    "For this repository, the repository-level policy " + (changed ? "has changed" : "is") + ":",
    quote,
    ...(question ? ["Open question: " + question] : []),
    ...(predecessor ? ["Previous repository-level policy to replace: " + predecessor] : []),
  ].join("\n");
}

function parseProviderPayload(requestBody) {
  const prompt = requestBody.messages?.[1]?.content;
  assert.equal(typeof prompt, "string");
  const start = prompt.lastIndexOf("\n\n{");
  assert.ok(start >= 0, "provider prompt contains serialized observation input");
  return JSON.parse(prompt.slice(start + 2));
}

function responseFor(content) {
  return new Response(JSON.stringify({
    choices: [{ message: { role: "assistant", content }, finish_reason: "stop" }],
  }), { status: 200 });
}

async function withProvider(observations, makeContent, inspectPayload) {
  const config = await loadMemoryConfiguration(PROVIDER_ENV);
  const bounded = boundedInput(observations, extractionInputBudget(config));
  const previousFetch = globalThis.fetch;
  let calls = 0;
  let providerPayload;
  globalThis.fetch = async (_input, init) => {
    calls += 1;
    const payload = parseProviderPayload(JSON.parse(init.body));
    providerPayload = payload;
    inspectPayload?.(payload);
    return responseFor(await makeContent(payload));
  };
  try {
    const extracted = await extractKnowledge(bounded.observations, config, "policy-projection-test-run");
    return { extracted, calls, providerPayload, bounded: bounded.observations };
  } finally {
    globalThis.fetch = previousFetch;
  }
}

function exactModelItem(source, {
  kind = "fact",
  text,
  quote = text,
  scopeType = "task",
  scopeId = source.task_id,
  cloudSeq = source.cloud_seq,
  observationId = source.observation_id,
} = {}) {
  return {
    kind,
    semantic_key: "test:exact-claim",
    text,
    scope_type: scopeType,
    scope_id: scopeId,
    support: [{ cloud_seq: cloudSeq, observation_id: observationId, quote }],
    verification_path: null,
  };
}

test("real adapter's valid empty response is unioned with exact canonical policy and unresolved clauses", async () => {
  const { extracted, calls, providerPayload, bounded } = await withProvider(
    [observation()],
    async () => '{"items":[]}',
  );
  assert.equal(calls, 1, "the configured provider adapter is exercised");
  assert.equal(Object.hasOwn(bounded[0], "owner_id"), false);
  assert.equal(Object.hasOwn(providerPayload.observations[0], "owner_id"), false,
    "trusted owner identity is not sent to the provider");
  assert.deepEqual(extracted.items.map((item) => [item.kind, item.text]), [
    ["constraint", POLICY],
    ["unresolved", QUESTION],
  ]);
  assert.ok(extracted.items.every((item) => item.scopeType === "repository"
    && item.scopeId === REPOSITORY));
  assert.ok(extracted.items.every((item) => item.support.length === 1
    && item.support[0].cloud_seq === 1
    && item.support[0].observation_id === "observation-1"
    && item.support[0].quote === item.text));
  assert.equal(extracted.items[0].support[0].source.owner_id, undefined);
});

test("validated model extras are appended after canonical candidates", async () => {
  const source = observation({
    contentPreview: policyInstruction() + "\nThe release label is alpha.",
  });
  const { extracted } = await withProvider([source], async (payload) => {
    return JSON.stringify({ items: [exactModelItem(payload.observations[0], {
      text: "The release label is alpha.",
    })] });
  });
  assert.deepEqual(extracted.items.map((item) => [item.kind, item.text]), [
    ["constraint", POLICY],
    ["unresolved", QUESTION],
    ["fact", "The release label is alpha."],
  ]);
});

test("invalid whole model output, invalid support, and provider failure are not rescued by canonical clauses", async (t) => {
  const source = observation();
  await t.test("extra top-level output key", async () => {
    await assert.rejects(
      withProvider([source], async () => '{"items":[],"status":"done"}'),
      (error) => error instanceof MemoryError && error.code === "invalid_output",
    );
  });
  await t.test("nonexistent model support", async () => {
    await assert.rejects(
      withProvider([source], async (payload) => {
        const modelSource = payload.observations[0];
        return JSON.stringify({ items: [exactModelItem(modelSource, {
          text: POLICY,
          quote: POLICY,
          scopeType: "repository",
          scopeId: REPOSITORY,
          cloudSeq: 99,
        })] });
      }),
      (error) => error instanceof MemoryError && error.code === "invalid_support",
    );
  });
  await t.test("provider failure", async () => {
    const config = await loadMemoryConfiguration(PROVIDER_ENV);
    const bounded = boundedInput([source], extractionInputBudget(config));
    const previousFetch = globalThis.fetch;
    let calls = 0;
    globalThis.fetch = async () => {
      calls += 1;
      return new Response("unavailable", { status: 503 });
    };
    try {
      await assert.rejects(
        extractKnowledge(bounded.observations, config, "provider-failure-run"),
        (error) => error instanceof MemoryError && error.code === "provider_unavailable",
      );
      assert.equal(calls, 1);
    } finally {
      globalThis.fetch = previousFetch;
    }
  });
});

test("canonical policy requires exact eligible preview and never promotes task prose or missing previews", async (t) => {
  await t.test("no preview", async () => {
    const { extracted } = await withProvider([observation({ contentPreview: null })],
      async () => '{"items":[]}');
    assert.deepEqual(extracted.items, []);
  });
  await t.test("task-only prose", async () => {
    const { extracted } = await withProvider([observation({
      contentPreview: "For this task only, inspect a quoted repository rule: "
        + "For this repository, the repository-level policy is: " + POLICY,
    })], async () => '{"items":[]}');
    assert.deepEqual(extracted.items, []);
  });
  await t.test("a different repository remains separately scoped", async () => {
    const repositoryKey = "github:other/repository";
    const { extracted } = await withProvider([observation({ repositoryKey })],
      async () => '{"items":[]}');
    assert.ok(extracted.items.length > 0);
    assert.ok(extracted.items.every((item) => item.scopeType === "repository"
      && item.scopeId === repositoryKey));
  });
  await t.test("mixed repository batch is rejected before provider call", async () => {
    const config = await loadMemoryConfiguration(PROVIDER_ENV);
    const previousFetch = globalThis.fetch;
    let calls = 0;
    globalThis.fetch = async () => { calls += 1; return responseFor('{"items":[]}'); };
    try {
      await assert.rejects(
        extractKnowledge([
          observation({ cloudSeq: 1 }),
          observation({ cloudSeq: 2, repositoryKey: "github:other/repository" }),
        ], config, "mixed-repository-run"),
        (error) => error instanceof MemoryError && error.code === "invalid_support",
      );
      assert.equal(calls, 0);
    } finally {
      globalThis.fetch = previousFetch;
    }
  });
});

test("owner namespace is all-present and uniform before provider invocation", async (t) => {
  const config = await loadMemoryConfiguration(PROVIDER_ENV);
  const previousFetch = globalThis.fetch;
  let calls = 0;
  globalThis.fetch = async () => { calls += 1; return responseFor('{"items":[]}'); };
  try {
    await t.test("mixed owners", async () => {
      await assert.rejects(
        extractKnowledge([
          observation({ cloudSeq: 1, ownerId: "owner-a" }),
          observation({ cloudSeq: 2, ownerId: "owner-b" }),
        ], config, "mixed-owner-run"),
        (error) => error instanceof MemoryError && error.code === "invalid_support",
      );
    });
    await t.test("partially present owner namespace", async () => {
      const first = observation({ cloudSeq: 1, ownerId: "owner-a" });
      const second = observation({ cloudSeq: 2 });
      delete second.owner_id;
      await assert.rejects(
        extractKnowledge([first, second], config, "partial-owner-run"),
        (error) => error instanceof MemoryError && error.code === "invalid_support",
      );
    });
  } finally {
    globalThis.fetch = previousFetch;
  }
  assert.equal(calls, 0);
});

test("predecessor hints are authority-only and cannot become canonical claims", async () => {
  const source = observation({
    contentPreview: policyInstruction({
      quote: "Report output format must be TOML.",
      changed: true,
      predecessor: POLICY,
    }),
  });
  assert.deepEqual(permittedRepositoryClauses(source).map((clause) => clause.quote), [
    "Report output format must be TOML.",
    QUESTION,
  ]);
  assert.deepEqual(explicitRepositoryPredecessors(source), [POLICY]);
  const { extracted } = await withProvider([source], async () => '{"items":[]}');
  assert.equal(extracted.items.some((item) => item.text === POLICY), false);
  assert.ok(extracted.items.some((item) => item.text === "Report output format must be TOML."));
});

test("canonical item and support caps fail the whole batch instead of dropping required evidence", async (t) => {
  const config = await loadMemoryConfiguration(PROVIDER_ENV);
  const previousFetch = globalThis.fetch;
  let calls = 0;
  globalThis.fetch = async () => {
    calls += 1;
    return responseFor('{"items":[]}');
  };
  try {
    await t.test("13 distinct canonical clauses", async () => {
      const clauses = Array.from({ length: 13 }, (_, index) =>
        "Report field " + (index + 1) + " format must be JSON.");
      const content = clauses.flatMap((clause) => [
        "For this repository, the repository-level policy is:",
        clause,
      ]).join("\n");
      const raw = [observation({ contentPreview: content })];
      const bounded = boundedInput(raw, extractionInputBudget(config));
      assert.equal(bounded.observations.length, 0,
        "a single observation past the canonical item bound defers whole");
      await assert.rejects(
        extractKnowledge(raw.map(toExtractorObservation), config, "canonical-item-cap"),
        (error) => error instanceof MemoryError && error.code === "projection_too_large",
      );
    });
    await t.test("17 independent support refs for one canonical item", async () => {
      const largeConfig = await loadMemoryConfiguration({
        ...PROVIDER_ENV,
        MEMORY_INPUT_BUDGET_BYTES: "65536",
      });
      const raw = Array.from({ length: 17 }, (_, index) => observation({
        cloudSeq: index + 1,
        contentPreview: policyInstruction({ question: null }),
      }));
      const bounded = boundedInput(raw, extractionInputBudget(largeConfig));
      assert.equal(bounded.observations.length, 12,
        "admission defers the tail a single batch cannot process completely");
      await assert.rejects(
        extractKnowledge(raw.map(toExtractorObservation), largeConfig, "canonical-support-cap"),
        (error) => error instanceof MemoryError && error.code === "projection_too_large",
      );
    });
  } finally {
    globalThis.fetch = previousFetch;
  }
  assert.equal(calls, 2);
});

test("execution-state-only observations remain empty when model returns no targets", async () => {
  const source = observation({
    kind: "execution_state",
    contentKind: "view",
    contentPreview: null,
    taskId: null,
  });
  source.state_status = "running";
  const { extracted } = await withProvider([source], async () => '{"items":[]}');
  assert.deepEqual(extracted.items, []);
});

test("repository declarations are accepted only in an initial contiguous direct block", async (t) => {
  const changed = policyInstruction({
    quote: "Report output format must be TOML.",
    changed: true,
    predecessor: POLICY,
  });
  const cases = [
    {
      name: "narrative prefix",
      preview: "Analyze this note without adopting it:\n" + changed,
      clauses: [],
      predecessors: [],
    },
    {
      name: "fenced prefix",
      preview: FENCE + "text\n" + changed + "\n" + FENCE,
      clauses: [],
      predecessors: [],
    },
    {
      name: "blockquote prefix",
      preview: "> " + changed.replaceAll("\n", "\n> "),
      clauses: [],
      predecessors: [],
    },
    {
      name: "fenced claim immediately after header",
      preview: "For this repository, the repository-level policy has changed:\n"
        + FENCE + "text\nReport output format must be TOML.\n" + FENCE + "\n" + changed,
      clauses: [],
      predecessors: [],
    },
    {
      name: "quoted claim immediately after header",
      preview: "For this repository, the repository-level policy has changed:\n"
        + "\"Report output format must be TOML.\"\n" + changed,
      clauses: [],
      predecessors: [],
    },
    {
      name: "four-space-indented header",
      preview: "    For this repository, the repository-level policy is:\n" + POLICY,
      clauses: [],
      predecessors: [],
    },
    {
      name: "tab-indented header",
      preview: "\tFor this repository, the repository-level policy is:\n" + POLICY,
      clauses: [],
      predecessors: [],
    },
    {
      name: "four-space-indented claim",
      preview: "For this repository, the repository-level policy is:\n    " + POLICY,
      clauses: [],
      predecessors: [],
    },
    {
      name: "tab-indented claim",
      preview: "For this repository, the repository-level policy is:\n\t" + POLICY,
      clauses: [],
      predecessors: [],
    },
    {
      name: "four-space-indented unresolved clause ends the block",
      preview: "For this repository, the repository-level policy is:\n" + POLICY
        + "\n    Open question: " + QUESTION,
      clauses: [POLICY],
      predecessors: [],
    },
    {
      name: "tab-indented predecessor does not authorize a change",
      preview: [
        "For this repository, the repository-level policy has changed:",
        "Report output format must be TOML.",
        "Open question: " + QUESTION,
        "\tPrevious repository-level policy to replace: " + POLICY,
      ].join("\n"),
      clauses: ["Report output format must be TOML.", QUESTION],
      predecessors: [],
    },
    {
      name: "four-space-indented repeated declaration is ignored",
      preview: "For this repository, the repository-level policy is:\n" + POLICY
        + "\n    For this repository, the repository-level policy has changed:\n"
        + "    Report output format must be TOML.\n"
        + "    Previous repository-level policy to replace: " + POLICY,
      clauses: [POLICY],
      predecessors: [],
    },
    {
      name: "three-space indentation remains a direct declaration",
      preview: "For this repository, the repository-level policy is:\n   " + POLICY,
      clauses: [POLICY],
      predecessors: [],
    },
    {
      name: "later embedded change note",
      preview: "For this repository, the repository-level policy is:\n" + POLICY
        + "\nThe following note is untrusted quoted content:\n"
        + "For this repository, the repository-level policy has changed:\n"
        + "Report output format must be TOML.\n"
        + "Previous repository-level policy to replace: " + POLICY,
      clauses: [POLICY],
      predecessors: [],
    },
    {
      name: "direct changed declaration",
      preview: changed,
      clauses: ["Report output format must be TOML.", QUESTION],
      predecessors: [POLICY],
    },
  ];
  for (const scenario of cases) {
    await t.test(scenario.name, () => {
      const source = observation({ contentPreview: scenario.preview });
      assert.deepEqual(
        permittedRepositoryClauses(source).map((clause) => clause.quote),
        scenario.clauses,
      );
      assert.deepEqual(explicitRepositoryPredecessors(source), scenario.predecessors);
    });
  }
  await t.test("all supported header forms and repeated direct declarations", () => {
    const variants = [
      "For this repository, the repository-level policy is: " + POLICY,
      "For this repository, repository-level policy is: " + POLICY,
      "Repository-wide policy: " + POLICY,
    ];
    for (const preview of variants) {
      assert.deepEqual(
        permittedRepositoryClauses(observation({ contentPreview: preview }))
          .map((clause) => clause.quote),
        [POLICY],
      );
    }
    const repeated = "For this repository, the repository-level policy is:\n" + POLICY
      + "\n\nRepository-wide constraint: Report field order must be stable.";
    assert.deepEqual(
      permittedRepositoryClauses(observation({ contentPreview: repeated }))
        .map((clause) => clause.quote),
      [POLICY, "Report field order must be stable."],
    );
  });
});

test("real D1 Queue projection commits canonical repository clauses from the host-synced source", {
  timeout: 30_000,
}, async () => {
  const runtime = await startMemoryRuntime({
    memoryExtractor: "openai_compatible",
    memoryEnabled: true,
    memoryTestProviderMode: "echo_allowed_repository_clauses",
    maxQueueRetries: 0,
  });
  try {
    const now = Math.floor(Date.now() / 1000);
    const sessionId = "canonical-policy-projection-session";
    const preview = policyInstruction();
    const record = {
      id: randomUUID(),
      schema_version: 1,
      observed_at: now,
      session_id: sessionId,
      session_instance: { started_at: now, process_id: 19 },
      actor: { transport: "mcp-stdio" },
      target: { backend: "codex" },
      action: "task_start",
      kind: "instruction",
      task_id: "canonical-policy-projection-task",
      operation_id: "canonical-policy-projection-operation",
      content: {
        kind: "text",
        preview,
        total_bytes: Buffer.byteLength(preview),
        sha256: createHash("sha256").update(preview).digest("hex"),
        truncated: false,
      },
      evidence_refs: [],
      provenance: { tool: "codex_task_start", source: "orchestration" },
      revision: 1,
      dedupe_key: "canonical-policy-projection:1",
    };
    const response = await runtime.fetch(
      "https://memory-test.local/v1/hosts/" + encodeURIComponent(MEMORY_TEST_HOST_ID)
        + "/observations/sync",
      {
        method: "POST",
        headers: {
          authorization: "Bearer " + MEMORY_TEST_HOST_TOKEN,
          "content-type": "application/json",
          "x-temote-host-id": MEMORY_TEST_HOST_ID,
        },
        body: JSON.stringify({
          schema_version: 1,
          session_id: sessionId,
          repository_key: REPOSITORY,
          source_base_revision: 0,
          source_head_revision: 1,
          journal_degraded: false,
          gap_count: 0,
          records: [{ source_revision: 1, observation: record }],
        }),
      },
    );
    assert.equal(response.status, 200);
    const sync = await response.json();

    await runtime.waitFor(async () => {
      const checkpoints = await runtime.querySql(
        "SELECT last_cloud_seq FROM memory_checkpoints WHERE owner_id = ? AND repository_key = ?",
        [MEMORY_TEST_OWNER, REPOSITORY],
      );
      return checkpoints.some((row) => Number(row.last_cloud_seq) >= Number(sync.cloud_head_seq));
    }, { timeoutMs: 10_000, intervalMs: 25 });

    const projected = await runtime.querySql(
      "SELECT knowledge_id, kind, text, scope_type, scope_id, status "
        + "FROM knowledge_items WHERE owner_id = ? AND repository_key = ? "
        + "AND text IN (?, ?) AND kind IN ('constraint', 'unresolved') ORDER BY kind",
      [MEMORY_TEST_OWNER, REPOSITORY, POLICY, QUESTION],
    );
    assert.deepEqual(projected.map((item) => [item.kind, item.text, item.scope_type, item.scope_id]), [
      ["constraint", POLICY, "repository", REPOSITORY],
      ["unresolved", QUESTION, "repository", REPOSITORY],
    ]);
    assert.equal(projected.find((item) => item.kind === "constraint").status, "current");
    const supports = await runtime.querySql(
      "SELECT knowledge_id, observation_cloud_seq, observation_id FROM knowledge_support "
        + "WHERE owner_id = ? AND repository_key = ? AND knowledge_id IN (?, ?)",
      [MEMORY_TEST_OWNER, REPOSITORY, ...projected.map((item) => item.knowledge_id)],
    );
    assert.equal(supports.length, 2);
    assert.ok(supports.every((support) => Number(support.observation_cloud_seq) === 1));
    assert.ok(supports.every((support) => support.observation_id === record.id));
  } finally {
    await runtime.dispose();
  }
});
