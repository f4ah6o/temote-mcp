const UUID = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i;
const text = value => typeof value === "string" && value.length > 0 && value.length <= 2048 && !value.includes("\0");

// Structural admission only: the selected Host owns canonical RepositoryId,
// root resolution, permission policy, and durable provisioning receipts.
export function validateSessionStart(args) {
  if (!args || typeof args !== "object" || Array.isArray(args)
    || Object.keys(args).some(key => !["host_id", "path", "source", "operation_id", "session_id"].includes(key))) {
    return "session_start accepts only host_id, path, source, operation_id, and session_id";
  }
  const path = Object.hasOwn(args, "path");
  const source = Object.hasOwn(args, "source");
  if (path === source) return "session_start requires exactly one of path or source";
  if (path) return text(args.path) ? null : "session_start requires a non-empty logical path";
  if (!UUID.test(args.operation_id ?? "")) return "managed source requires a UUID operation_id";
  if (typeof args.source === "string") return text(args.source) ? null : "invalid repository source";
  const value = args.source;
  if (!value || typeof value !== "object" || Array.isArray(value)
    || Object.keys(value).some(key => !["kind", "repository", "base", "vcs"].includes(key))
    || value.kind !== "repository" || !text(value.repository)
    || (Object.hasOwn(value, "base") && !text(value.base))
    || (Object.hasOwn(value, "vcs") && !["auto", "jujutsu", "git"].includes(value.vcs))) return "invalid typed repository source";
  return null;
}
