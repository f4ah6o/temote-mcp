import assert from "node:assert/strict";
import test from "node:test";

import { loadMemoryConfiguration } from "../src/memory/config.js";
import { extractKnowledge, MemoryError } from "../src/memory/extractor.js";

const PROVIDER_ENV = Object.freeze({
  MEMORY_ENABLED: "true",
  MEMORY_EXTRACTOR: "openai_compatible",
  MEMORY_ENDPOINT: "https://extractor.example/v1/chat/completions",
  MEMORY_MODEL: "bounded-test-model",
  MEMORY_API_KEY: "test-only-provider-key",
  MEMORY_OUTPUT_BUDGET_BYTES: "8192",
});

function envelope(content, extra = {}) {
  return {
    id: "response-id",
    choices: [{ message: { role: "assistant", content }, finish_reason: "stop" }],
    ...extra,
  };
}

function streamResponse(chunks, { cancel } = {}) {
  let index = 0;
  const body = new ReadableStream({
    pull(controller) {
      if (index < chunks.length) controller.enqueue(chunks[index++]);
      else if (!cancel) controller.close();
    },
    cancel,
  });
  return new Response(body, { status: 200 });
}

function chunkBytes(bytes, size) {
  const chunks = [];
  for (let start = 0; start < bytes.byteLength; start += size) {
    chunks.push(bytes.slice(start, start + size));
  }
  return chunks;
}

async function extractWithFetch(config, responseFactory, inspectRequest) {
  const previousFetch = globalThis.fetch;
  let calls = 0;
  globalThis.fetch = async (input, init) => {
    calls += 1;
    const request = {
      url: String(input),
      init,
      body: JSON.parse(init.body),
    };
    inspectRequest?.(request);
    return responseFactory(request);
  };
  try {
    const result = await extractKnowledge([{
      cloud_seq: 1,
      owner_id: "owner-a",
      repository_key: "github:example/repository",
      observation_id: "observation-a",
      kind: "instruction",
      content_kind: "text",
      content_preview: "A synthetic observation for adapter boundary tests.",
      evidence_refs: "[]",
    }], config, "stable-test-run");
    return { result, calls };
  } finally {
    globalThis.fetch = previousFetch;
  }
}

async function errorWithFetch(config, responseFactory, expectedCode) {
  await assert.rejects(
    extractWithFetch(config, responseFactory),
    (error) => error instanceof MemoryError && error.code === expectedCode,
  );
}

test("provider envelope has a larger bounded budget than validated message content", async () => {
  const config = await loadMemoryConfiguration(PROVIDER_ENV);
  const sentinel = "IGNORED_REASONING_SENTINEL";
  const body = JSON.stringify(envelope('{"items":[]}', {
    choices: [{
      message: {
        role: "assistant",
        content: '{"items":[]}',
        reasoning_content: (sentinel + "理由🫥").repeat(500),
      },
      finish_reason: "stop",
    }],
    usage: { completion_tokens: 1234, private_metadata: sentinel },
  }));
  assert.ok(Buffer.byteLength(body) > config.outputBudgetBytes);
  assert.ok(Buffer.byteLength(body) <= config.envelopeBudgetBytes);

  const bytes = new TextEncoder().encode(body);
  const { result, calls } = await extractWithFetch(
    config,
    () => streamResponse(chunkBytes(bytes, 7)),
    ({ body: requestBody }) => {
      assert.equal(requestBody.stream, false);
      assert.equal(requestBody.max_tokens, 2048);
      assert.equal(Object.hasOwn(requestBody, "reasoning_effort"), false);
    },
  );
  assert.equal(calls, 1);
  assert.deepEqual(result.items, []);
  assert.equal(JSON.stringify(result).includes(sentinel), false);
});

test("provider envelope overflow aborts and cancels the streamed response", async () => {
  const config = await loadMemoryConfiguration({
    ...PROVIDER_ENV,
    MEMORY_OUTPUT_BUDGET_BYTES: "32768",
  });
  let cancelled = false;
  let requestSignal;
  const chunks = [
    new Uint8Array(config.envelopeBudgetBytes),
    new Uint8Array([0x78]),
  ];
  await assert.rejects(
    extractWithFetch(config, () => streamResponse(chunks, {
      cancel() { cancelled = true; },
    }), ({ init }) => { requestSignal = init.signal; }),
    (error) => error instanceof MemoryError && error.code === "provider_envelope_too_large",
  );
  assert.equal(cancelled, true);
  assert.equal(requestSignal.aborted, true);
});

test("message content is capped by UTF-8 bytes independently of the JSON envelope", async (t) => {
  const config = await loadMemoryConfiguration(PROVIDER_ENV);
  const tooManyAsciiBytes = "x".repeat(config.outputBudgetBytes + 1);
  const multibyteText = "界".repeat(Math.ceil(config.outputBudgetBytes / 3));
  assert.ok(multibyteText.length < config.outputBudgetBytes);
  assert.ok(new TextEncoder().encode(multibyteText).byteLength > config.outputBudgetBytes);

  await t.test("ASCII content over the limit", async () => {
    await errorWithFetch(config, () => new Response(JSON.stringify(envelope(tooManyAsciiBytes))),
      "provider_output_too_large");
  });
  await t.test("multibyte content over the byte limit", async () => {
    await errorWithFetch(config, () => new Response(JSON.stringify(envelope(multibyteText))),
      "provider_output_too_large");
  });
});

test("no-stream response fallback remains bounded and compatible", async () => {
  const config = await loadMemoryConfiguration(PROVIDER_ENV);
  const raw = JSON.stringify(envelope('{"items":[]}'));
  const { result, calls } = await extractWithFetch(config, () => ({
    ok: true,
    status: 200,
    body: null,
    headers: new Headers(),
    async text() { return raw; },
  }));
  assert.equal(calls, 1);
  assert.deepEqual(result.items, []);
});

test("finish_reason length is an incomplete response, while an absent reason remains compatible", async (t) => {
  const config = await loadMemoryConfiguration(PROVIDER_ENV);
  await t.test("truncated generation is rejected", async () => {
    await errorWithFetch(config, () => new Response(JSON.stringify({
      choices: [{ message: { content: '{"items":[]}' }, finish_reason: "length" }],
    })), "provider_incomplete_response");
  });
  await t.test("older adapters without finish_reason remain accepted", async () => {
    const { result } = await extractWithFetch(config, () => new Response(JSON.stringify({
      choices: [{ message: { content: '{"items":[]}' } }],
    })));
    assert.deepEqual(result.items, []);
  });
});

test("reasoning effort is optional, validated, sent only when configured, and versioned", async (t) => {
  const absent = await loadMemoryConfiguration(PROVIDER_ENV);
  const low = await loadMemoryConfiguration({
    ...PROVIDER_ENV,
    MEMORY_REASONING_EFFORT: "low",
  });
  assert.equal(absent.reasoningEffort, null);
  assert.equal(absent.envelopeBudgetBytes, 2 * absent.outputBudgetBytes + 16_384);
  assert.equal(low.reasoningEffort, "low");
  assert.notEqual(low.producerVersion, absent.producerVersion);

  await t.test("configured low effort is forwarded without changing the token budget", async () => {
    const { result, calls } = await extractWithFetch(low,
      () => new Response(JSON.stringify(envelope('{"items":[]}'))),
      ({ body }) => {
        assert.equal(body.reasoning_effort, "low");
        assert.equal(body.max_tokens, 2048);
      });
    assert.equal(calls, 1);
    assert.deepEqual(result.items, []);
  });

  await t.test("unknown effort is a stable configuration error and sends no request", async () => {
    const invalid = await loadMemoryConfiguration({
      ...PROVIDER_ENV,
      MEMORY_REASONING_EFFORT: "disable-thinking",
    });
    assert.equal(invalid.configured, false);
    assert.equal(invalid.errorCode, "provider_configuration_invalid");
    assert.equal(invalid.producerVersion, null);
    const previousFetch = globalThis.fetch;
    let calls = 0;
    globalThis.fetch = async () => { calls += 1; throw new Error("unexpected provider call"); };
    try {
      await assert.rejects(
        extractKnowledge([{
          cloud_seq: 1,
          owner_id: "owner-a",
          repository_key: "github:example/repository",
          observation_id: "observation-a",
          kind: "instruction",
        }], invalid, "stable-test-run"),
        (error) => error instanceof MemoryError && error.code === "provider_configuration_invalid",
      );
    } finally {
      globalThis.fetch = previousFetch;
    }
    assert.equal(calls, 0);
  });
});
