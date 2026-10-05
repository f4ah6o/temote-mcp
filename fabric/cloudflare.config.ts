import { readFileSync } from "node:fs";
import { bindings, defineConfig, exports, triggers } from "cf/config";
import { deploymentPlan, namespaceExports } from "./deployment-plan.mjs";

// Deployment identifiers and Access policy values belong in an ignored local
// profile. Secrets stay in Cloudflare/1Password; this file declares names only.
const profilePath = process.env.TEMOTE_DEPLOYMENT_CONFIG;
const profile = profilePath ? JSON.parse(readFileSync(profilePath, "utf8")) : {};
const plan = deploymentPlan(profile);
const workerName = plan.workerName;
const compatibilitySecrets = profile.compatibilitySecrets ?? [];
if (!Array.isArray(compatibilitySecrets) || new Set(compatibilitySecrets).size !== compatibilitySecrets.length
    || compatibilitySecrets.some(name => !["HOST_TOKEN", "CLIENT_TOKEN", "FABRIC_INTERACTION_SECRET"].includes(name))) {
  throw new Error("Unsupported compatibility secret binding names");
}
const variables = {
  ACCESS_TEAM_DOMAIN: "replace-me.cloudflareaccess.com",
  ACCESS_AUDIENCE: "replace-with-access-application-aud",
  ACCESS_ALLOWED_EMAILS: "replace@example.com",
  OBSERVATION_OWNER_ID: "replace-with-owner-id",
  MEMORY_ENABLED: "false",
  MEMORY_EXTRACTOR: "openai_compatible",
  MEMORY_ENDPOINT: "https://api.openai.com/v1/chat/completions",
  MEMORY_MODEL: "replace-with-available-model",
  MEMORY_TIMEOUT_MS: "30000",
  MEMORY_INPUT_BUDGET_BYTES: "32768",
  MEMORY_OUTPUT_BUDGET_BYTES: "8192",
  MEMORY_MAX_ATTEMPTS: "3",
  MEMORY_BATCH_SIZE: "32",
  MEMORY_PROJECTION_GENERATION: "1",
  ...profile.vars,
};
for (const [name, value] of Object.entries(variables)) {
  if (typeof value !== "string" || /(?:TOKEN|SECRET|API_KEY|PASSWORD)/i.test(name)) {
    throw new Error("Deployment profile vars must contain only non-secret strings");
  }
}
const requiredVariables = ["ACCESS_TEAM_DOMAIN", "ACCESS_AUDIENCE", "ACCESS_ALLOWED_EMAILS", "OBSERVATION_OWNER_ID"];
if (variables.MEMORY_ENABLED === "true") requiredVariables.push("MEMORY_MODEL", "MEMORY_ENDPOINT");
if (profilePath && (!/^[0-9a-f-]{36}$/.test(profile.databaseId ?? "") ||
    (!["prepare", "transfer"].includes(plan.stage) && !/^(?:[a-z0-9](?:[a-z0-9-]*[a-z0-9])?\.)+[a-z0-9-]+$/.test(profile.domain ?? "")) ||
    (profile.domain && !/^(?:[a-z0-9](?:[a-z0-9-]*[a-z0-9])?\.)+[a-z0-9-]+$/.test(profile.domain)) ||
    requiredVariables.some(name => !variables[name] || variables[name].startsWith("replace-")))) {
  throw new Error("Deployment profile requires a database, domain, and complete non-secret configuration");
}

const memoryEnabled = variables.MEMORY_ENABLED === "true";
const eventsEnabled = variables.EVENTS_ENABLED === "true";
if (eventsEnabled && !/^https:\/\//.test(variables.EVENT_SENDER_URL ?? "")) throw new Error("Events require an HTTPS sender origin");
export default defineConfig({
  worker: {
    name: workerName,
    compatibilityDate: "2026-08-01",
    entrypoint: plan.entrypoint,
    workersDev: false,
    previewUrls: false,
    ...(profile.domain ? { domains: [profile.domain] } : {}),
    assets: { htmlHandling: "none", notFoundHandling: "none", runWorkerFirst: true },
    triggers: plan.frontdoor ? [] : [
      ...((memoryEnabled || eventsEnabled) ? [triggers.scheduled({ schedule: "*/5 * * * *" })] : []),
      ...(memoryEnabled ? [triggers.queue({ name: "temote-memory", maxBatchSize: 1, maxBatchTimeout: 5, maxRetries: 3 })] : []),
    ],
    env: {
      ...Object.fromEntries(Object.entries(variables).map(([name, value]) => [name, bindings.text(value)])),
      ...(plan.frontdoor ? { FABRIC_AUTHORITY: bindings.worker({ worker: plan.source }) } : { HOST_TOKENS_JSON: bindings.secret() }),
      ...(!plan.frontdoor ? Object.fromEntries(compatibilitySecrets.map(name => [name, bindings.secret()])) : {}),
      ...(eventsEnabled && !plan.frontdoor ? {
        EVENT_SENDER_BEARER: bindings.secret(),
        EVENT_SENDER_ACCESS_CLIENT_ID: bindings.secret(),
        EVENT_SENDER_ACCESS_CLIENT_SECRET: bindings.secret(),
      } : {}),
      ...(memoryEnabled ? {
        MEMORY_API_KEY: bindings.secret(),
        MEMORY_QUEUE: bindings.queue({ name: "temote-memory" }),
      } : {}),
      OBSERVATION_DB: bindings.d1({ name: "temote-observation", ...(profile.databaseId ? { id: profile.databaseId } : {}) }),
      GATEWAY_SESSIONS: bindings.durableObject({ worker: plan.namespaceOwner, exportName: plan.sessionClass }),
      GATEWAY_REGISTRY: bindings.durableObject({ worker: plan.namespaceOwner, exportName: plan.registryClass }),
      FABRIC_SESSIONS: bindings.durableObject({ worker: plan.namespaceOwner, exportName: plan.sessionClass }),
      FABRIC_REGISTRY: bindings.durableObject({ worker: plan.namespaceOwner, exportName: plan.registryClass }),
      FABRIC_DEPLOYMENT: bindings.versionMetadata(),
      ASSETS: bindings.assets(),
    },
    exports: namespaceExports(plan, exports),
  },
});
