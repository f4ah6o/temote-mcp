import { mkdtemp, rm } from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";

import { unstable_dev } from "wrangler";

const GATEWAY_DIR = fileURLToPath(new URL("..", import.meta.url));
const WORKER_SCRIPT = path.join(GATEWAY_DIR, "src/index.js");
const WRANGLER_CONFIG = path.join(GATEWAY_DIR, "wrangler.toml");

export const DASHBOARD_RUNTIME_CLIENT_TOKEN = "dashboard-runtime-test-client-token";
export const DASHBOARD_RUNTIME_HOST_TOKEN = "dashboard-runtime-test-host-token";

export async function startDashboardRuntime() {
  const persistencePath = await mkdtemp(path.join(os.tmpdir(), "temote-dashboard-runtime-"));
  let worker;

  try {
    worker = await unstable_dev(WORKER_SCRIPT, {
      config: WRANGLER_CONFIG,
      local: true,
      ip: "127.0.0.1",
      port: 0,
      persist: true,
      persistTo: persistencePath,
      vars: {
        CLIENT_TOKEN: DASHBOARD_RUNTIME_CLIENT_TOKEN,
        HOST_TOKEN: DASHBOARD_RUNTIME_HOST_TOKEN,
        HOST_TOKENS_JSON: JSON.stringify({ "runtime-host": DASHBOARD_RUNTIME_HOST_TOKEN }),
        ACCESS_TEAM_DOMAIN: "runtime.invalid.cloudflareaccess.com",
        ACCESS_AUDIENCE: "dashboard-runtime-test-audience",
        ACCESS_ALLOWED_EMAILS: "dashboard-runtime-test@example.com",
      },
      logLevel: "error",
      experimental: {
        disableDevRegistry: true,
        disableExperimentalWarning: true,
        showInteractiveDevSession: false,
      },
    });
  } catch (error) {
    await rm(persistencePath, { recursive: true, force: true });
    throw error;
  }

  return {
    fetch(input, init = {}) {
      return worker.fetch(input, { ...init, redirect: "manual" });
    },
    async close() {
      try {
        await worker.stop();
      } finally {
        await rm(persistencePath, { recursive: true, force: true });
      }
    },
  };
}
