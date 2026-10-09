// Explicit non-destructive migration stages. No stage deletes a namespace.
export function deploymentPlan(profile = {}) {
  const source = profile.sourceWorker ?? "temote-mcp-gateway";
  const target = profile.targetWorker ?? "temote-fabric";
  const stage = profile.migrationStage ?? "legacy";
  const validName = value => typeof value === "string" && /^[a-z0-9][a-z0-9-]{0,62}$/.test(value);
  if (!validName(source) || !validName(target) || source === target) throw new Error("Invalid distinct migration Worker names");
  if (!["legacy", "rename", "prepare", "transfer", "fabric"].includes(stage)) throw new Error("Unknown migration stage");
  const frontdoor = stage === "prepare" || stage === "fabric";
  const workerName = profile.workerName ?? (frontdoor ? target : source);
  if (workerName !== (frontdoor ? target : source)) throw new Error("Worker name does not match migration stage");
  if (stage === "prepare" && profile.domain) throw new Error("Transfer preparation must not move the public domain");
  return {
    stage, source, target, workerName, frontdoor,
    namespaceOwner: stage === "transfer" || stage === "fabric" ? target : source,
    sessionClass: stage === "legacy" ? "GatewaySession" : "FabricSession",
    registryClass: stage === "legacy" ? "GatewayRegistry" : "FabricRegistry",
    entrypoint: stage === "legacy" ? "src/index.js"
      : stage === "rename" ? "src/deployment/canonical.js"
      : stage === "transfer" ? "src/deployment/authority.js" : "src/deployment/frontdoor.js",
  };
}

export function namespaceExports(plan, exports) {
  const created = () => exports.durableObject({ storage: "sqlite" });
  if (plan.stage === "legacy") return { GatewaySession: created(), GatewayRegistry: created() };
  if (plan.stage === "rename") return {
    GatewaySession: exports.durableObject({ state: "renamed", renamedTo: "FabricSession" }),
    GatewayRegistry: exports.durableObject({ state: "renamed", renamedTo: "FabricRegistry" }),
    FabricSession: created(), FabricRegistry: created(),
  };
  const definition = plan.stage === "prepare"
    ? { state: "expecting-transfer", storage: "sqlite", transferFrom: plan.source }
    : plan.stage === "transfer" ? { state: "transferred", transferredTo: plan.target } : { storage: "sqlite" };
  return { FabricSession: exports.durableObject(definition), FabricRegistry: exports.durableObject(definition) };
}
