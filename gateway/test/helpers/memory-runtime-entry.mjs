import production from "../../src/index.js";

export { GatewayRegistry, GatewaySession } from "../../src/routing-runtime.js";

let remainingTestQueueSendFailures;
let remainingTestQueueAckFailures;

function testQueueFailureEnv(env) {
  const raw = env.MEMORY_TEST_QUEUE_SEND_FAILURES;
  if (typeof raw !== "string") return env;
  if (remainingTestQueueSendFailures === undefined) {
    const parsed = Number(raw);
    remainingTestQueueSendFailures = Number.isSafeInteger(parsed) && parsed > 0
      ? Math.min(parsed, 16)
      : 0;
  }
  const queue = env.MEMORY_QUEUE;
  const testQueue = {
    async send(...args) {
      if (remainingTestQueueSendFailures > 0) {
        remainingTestQueueSendFailures -= 1;
        throw new Error("memory_test_queue_send_failure");
      }
      return queue.send(...args);
    },
  };
  return new Proxy(env, {
    get(target, property, receiver) {
      if (property === "MEMORY_QUEUE") return testQueue;
      return Reflect.get(target, property, receiver);
    },
  });
}

function testQueueAckFailureBatch(batch, env) {
  const raw = env.MEMORY_TEST_QUEUE_ACK_FAILURES;
  if (typeof raw !== "string") return batch;
  if (remainingTestQueueAckFailures === undefined) {
    const parsed = Number(raw);
    remainingTestQueueAckFailures = Number.isSafeInteger(parsed) && parsed > 0
      ? Math.min(parsed, 16)
      : 0;
  }
  if (remainingTestQueueAckFailures <= 0) return batch;
  return {
    ...batch,
    messages: (batch.messages ?? []).map((message) => ({
      body: message.body,
      ack() {
        if (remainingTestQueueAckFailures > 0) {
          remainingTestQueueAckFailures -= 1;
          throw new Error("memory_test_queue_ack_failure");
        }
        return message.ack?.();
      },
      retry(options) {
        return message.retry?.(options);
      },
    })),
  };
}

async function withTestProvider(env, callback) {
  if (!new Set(["echo_allowed_repository_clauses", "reject"]).has(env.MEMORY_TEST_PROVIDER_MODE)) {
    return callback();
  }
  const endpoint = env.MEMORY_ENDPOINT;
  const originalFetch = globalThis.fetch;
  globalThis.fetch = async (input, init) => {
    if (String(input) !== endpoint) return originalFetch(input, init);
    if (env.MEMORY_TEST_PROVIDER_MODE === "reject") {
      return new Response("test provider unavailable", { status: 503 });
    }
    try {
      const request = JSON.parse(init?.body ?? "{}");
      const prompt = request.messages?.[1]?.content;
      if (typeof prompt !== "string") return new Response("invalid test prompt", { status: 400 });
      const payloadStart = prompt.lastIndexOf("\n{");
      if (payloadStart < 0) return new Response("invalid test payload", { status: 400 });
      const payload = JSON.parse(prompt.slice(payloadStart + 1));
      const items = [];
      for (const observation of payload.observations ?? []) {
        for (const clause of observation.allowed_repository_clauses ?? []) {
          items.push({
            kind: clause.kind,
            semantic_key: clause.subject,
            text: clause.quote,
            scope_type: clause.scope_type,
            scope_id: clause.scope_id,
            support: [{
              cloud_seq: observation.cloud_seq,
              observation_id: observation.observation_id,
              quote: clause.quote,
            }],
            verification_path: null,
          });
        }
      }
      return Response.json({
        choices: [{ message: { content: JSON.stringify({ items }) } }],
      });
    } catch {
      return new Response("invalid test provider request", { status: 400 });
    }
  };
  try {
    return await callback();
  } finally {
    globalThis.fetch = originalFetch;
  }
}

async function admin(request, env) {
  try {
    const url = new URL(request.url);
    if (request.method !== "POST") return new Response("method not allowed", { status: 405 });
    const input = await request.json();
    if (url.pathname === "/__memory_test/exec") {
      const statement = env.OBSERVATION_DB.prepare(input.sql);
      const result = Array.isArray(input.params) && input.params.length
        ? await statement.bind(...input.params).run()
        : await statement.run();
      return Response.json({ success: true, result });
    }
    if (url.pathname === "/__memory_test/batch") {
      if (!Array.isArray(input.statements) || input.statements.length > 1000) {
        return Response.json({ success: false, error: "invalid_batch" }, { status: 400 });
      }
      const statements = input.statements.map(({ sql, params = [] }) =>
        env.OBSERVATION_DB.prepare(sql).bind(...params));
      const results = await env.OBSERVATION_DB.batch(statements);
      return Response.json({ success: true, results });
    }
    if (url.pathname === "/__memory_test/query") {
      const result = await env.OBSERVATION_DB.prepare(input.sql).bind(...(input.params ?? [])).all();
      return Response.json({ success: true, result });
    }
    if (url.pathname === "/__memory_test/queue") {
      await env.MEMORY_QUEUE.send(input.body);
      return Response.json({ success: true });
    }
    if (url.pathname === "/__memory_test/scheduled") {
      if (typeof production.scheduled !== "function") {
        return Response.json({ success: true, state: "not_implemented" });
      }
      const result = await production.scheduled(
        { cron: "*/5 * * * *", scheduledTime: Date.now() }, testQueueFailureEnv(env),
      );
      return Response.json({ success: true, state: "completed", result });
    }
    if (url.pathname === "/__memory_test/health") {
      const result = await env.OBSERVATION_DB.prepare("SELECT 1 AS ok").first();
      return Response.json({ success: result?.ok === 1 });
    }
    return new Response("not found", { status: 404 });
  } catch (error) {
    const message = String(error?.message ?? "admin_error").slice(0, 512);
    return Response.json({ success: false, error: message }, { status: 500 });
  }
}

export default {
  async fetch(request, env, ctx) {
    if (new URL(request.url).pathname.startsWith("/__memory_test/")) return admin(request, env);
    return production.fetch(request, testQueueFailureEnv(env), ctx);
  },
  async queue(batch, env, ctx) {
    const marker = await env.OBSERVATION_DB.prepare(
      "INSERT INTO __memory_test_queue_dispatches (received_at, message_count, state) VALUES (?, ?, 'running')",
    ).bind(new Date().toISOString(), Array.isArray(batch?.messages) ? batch.messages.length : 0).run();
    const id = Number(marker.meta?.last_row_id);
    try {
      if (typeof production.queue !== "function") {
        for (const message of batch?.messages ?? []) message.ack?.();
        await env.OBSERVATION_DB.prepare(
          "UPDATE __memory_test_queue_dispatches SET state = 'completed', completed_at = ? WHERE dispatch_id = ?",
        ).bind(new Date().toISOString(), id).run();
        return { state: "not_implemented" };
      }
      const queueEnv = testQueueFailureEnv(env);
      const queueBatch = testQueueAckFailureBatch(batch, env);
      const result = await withTestProvider(queueEnv, () => production.queue(queueBatch, queueEnv, ctx));
      await env.OBSERVATION_DB.prepare(
        "UPDATE __memory_test_queue_dispatches SET state = 'completed', completed_at = ? WHERE dispatch_id = ?",
      ).bind(new Date().toISOString(), id).run();
      return result;
    } catch (error) {
      await env.OBSERVATION_DB.prepare(
        "UPDATE __memory_test_queue_dispatches SET state = 'failed', completed_at = ? WHERE dispatch_id = ?",
      ).bind(new Date().toISOString(), id).run().catch(() => {});
      throw error;
    }
  },
  async scheduled(controller, env, ctx) {
    if (typeof production.scheduled !== "function") return;
    return production.scheduled(controller, testQueueFailureEnv(env), ctx);
  },
};
