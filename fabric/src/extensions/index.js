import { rpcError, rpcResult } from "../protocol.js";
import { MENTION_TOOL, MENTION_TOOL_NAME, searchMentions } from "./mentions.js";
import { INTERACTION_TOOL, INTERACTION_TOOL_NAME } from "./interactions.js";

export { MENTION_TOOL, MENTION_TOOL_NAME, searchMentions } from "./mentions.js";
export { handleInteractionCall, INTERACTION_TOOL_NAME, supportsOpenAIForm } from "./interactions.js";

export const EXTENSION_CONTRACT_VERSION = 2;

export function extensionTools({ mentions = false, interactions = false } = {}) {
  return [...(mentions ? [MENTION_TOOL] : []), ...(interactions ? [INTERACTION_TOOL] : [])];
}

// Keep this independent of publicContractFingerprint(): optional presentation
// metadata never changes the standard routed-tool contract digest.
export async function extensionContractFingerprint() {
  const contract = {
    version: EXTENSION_CONTRACT_VERSION,
    mentionTool: MENTION_TOOL,
    interactionTool: INTERACTION_TOOL,
    formMethod: "openai/elicitation/create",
    formCapability: "openai/elicitation",
  };
  const canonical = (value) => Array.isArray(value)
    ? `[${value.map(canonical).join(",")}]`
    : value && typeof value === "object"
      ? `{${Object.keys(value).sort().map((key) => `${JSON.stringify(key)}:${canonical(value[key])}`).join(",")}}`
      : JSON.stringify(value);
  const digest = await crypto.subtle.digest("SHA-256", new TextEncoder().encode(canonical(contract)));
  return [...new Uint8Array(digest)].map((byte) => byte.toString(16).padStart(2, "0")).join("");
}

export async function handleExtensionToolCall(rpc, env, identity) {
  if (rpc?.params?.name !== MENTION_TOOL_NAME) return null;
  let found;
  try {
    found = await searchMentions(rpc.params.arguments, identity, env);
  } catch {
    return rpcError(rpc.id, -32001, "mention_source_unavailable");
  }
  if (!found.ok) {
    return rpcError(rpc.id, found.code === "invalid_arguments" ? -32602 : -32001, found.code);
  }
  return rpcResult(rpc.id, { content: [], structuredContent: { items: found.items } });
}
