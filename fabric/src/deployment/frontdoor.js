export { GatewaySession as FabricSession, GatewayRegistry as FabricRegistry } from "../routing-runtime.js";

// Preserve the existing credential authority through an account-local Service
// binding. No credential is exported, copied, or accepted from configuration.
export default {
  async fetch(request, env) {
    try {
      if (!env.FABRIC_AUTHORITY?.fetch) throw new Error("unavailable");
      return await env.FABRIC_AUTHORITY.fetch(request);
    } catch {
      return Response.json({ error: "fabric_authority_unavailable" }, { status: 503, headers: { "cache-control": "no-store" } });
    }
  },
};
