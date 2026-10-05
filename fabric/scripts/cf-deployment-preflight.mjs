import { readFile } from "node:fs/promises";
import { pathToFileURL } from "node:url";
import { resolve } from "node:path";
import { deploymentPlan } from "../deployment-plan.mjs";

// Local plan validation does not claim that a remote migration has occurred.
// Supply a fresh namespace inventory from `cf durable-objects namespaces list`.
export function evaluateCfPreflight(profile, baseline, namespaces) {
  const plan = deploymentPlan(profile);
  if (!Array.isArray(namespaces) || namespaces.length > 1000) throw new Error("invalid_namespace_inventory");
  if (profile.databaseId !== baseline.databaseId || !/^[a-f0-9-]{36}$/.test(profile.databaseId ?? "")) {
    throw new Error("database_identity_mismatch");
  }
  const checked = [];
  for (const [binding, legacy, canonical] of [
    ["GATEWAY_SESSIONS", "GatewaySession", "FabricSession"],
    ["GATEWAY_REGISTRY", "GatewayRegistry", "FabricRegistry"],
  ]) {
    const expectedId = baseline.namespaces?.[binding];
    const matches = namespaces.filter(item => item.id === expectedId);
    if (typeof expectedId !== "string" || matches.length !== 1) throw new Error("namespace_identity_missing_or_ambiguous");
    const current = matches[0];
    if (![plan.source, plan.target].includes(current.script) || ![legacy, canonical].includes(current.class)
        || current.use_sqlite !== true) throw new Error("namespace_owner_or_class_mismatch");
    // Never approve a backwards configuration that could create fresh legacy
    // classes or replace the live target with transfer preparation. Target
    // preparation itself also requires a successful provider deployment.
    if (["legacy", "rename"].includes(plan.stage) && current.script !== plan.source) throw new Error("unexpected_transferred_owner");
    if (plan.stage === "legacy" && current.class !== legacy) throw new Error("legacy_configuration_after_rename");
    if (plan.stage === "prepare" && current.script !== plan.source) throw new Error("preparation_after_transfer");
    if (["prepare", "transfer", "fabric"].includes(plan.stage) && current.class !== canonical) throw new Error("class_rename_not_confirmed");
    if (plan.stage === "fabric" && current.script !== plan.target) throw new Error("namespace_transfer_not_confirmed");
    checked.push({ binding, namespace_id: expectedId, owner: current.script, class: current.class });
  }
  return { status: "local_plan_valid", remote_deployment: "not_run", stage: plan.stage,
    worker: plan.workerName, database_id: profile.databaseId, namespaces: checked };
}

if (process.argv[1] && import.meta.url === pathToFileURL(resolve(process.argv[1])).href) {
  try {
    const files = process.argv.slice(2);
    if (files.length !== 3) throw new Error("usage: cf-deployment-preflight.mjs PROFILE BASELINE NAMESPACE_INVENTORY");
    const [profile, baseline, inventory] = await Promise.all(files.map(async file => JSON.parse(await readFile(file, "utf8"))));
    console.log(JSON.stringify(evaluateCfPreflight(profile, baseline, inventory)));
  } catch {
    // JSON parsing and provider-shaped input may contain confidential text.
    console.error("cf_deployment_preflight_failed");
    process.exitCode = 1;
  }
}
