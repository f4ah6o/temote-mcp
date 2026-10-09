import { validateHostId, validateSessionId } from "../protocol.js";

const hostId = { type: "string", pattern: "^(?=.{1,128}$)(?=.*[A-Za-z0-9])[A-Za-z0-9._-]+$" };
const sessionId = { type: "string", pattern: "^(?!\\.{1,2}$)[A-Za-z0-9._-]{1,64}$" };
const jobId = { type: "string", pattern: "^[A-Za-z0-9._:-]{1,128}$" };
const jobState = { type: "string", enum: ["running", "completed", "failed", "stopped", "unknown"] };
const sessionState = { type: "string", enum: ["starting", "active", "stopping", "stopped", "crashed", "failed"] };
const timestamp = { type: "string", format: "date-time" };

export const EVENT_CATALOG = Object.freeze([
  {
    name: "job.state.changed",
    description: "A sandbox job in the selected Temote session changed state.",
    delivery: ["webhook"],
    inputSchema: { type: "object", properties: { host_id: hostId, session_id: sessionId, job_id: jobId }, required: ["host_id", "session_id"], additionalProperties: false },
    payloadSchema: { type: "object", properties: { host_id: hostId, session_id: sessionId, job_id: jobId, previous_state: { anyOf: [jobState, { type: "null" }] }, state: jobState, timestamp }, required: ["host_id", "session_id", "job_id", "previous_state", "state", "timestamp"], additionalProperties: false },
  },
  {
    name: "session.state.changed",
    description: "The selected Temote session changed lifecycle state.",
    delivery: ["webhook"],
    inputSchema: { type: "object", properties: { host_id: hostId, session_id: sessionId }, required: ["host_id", "session_id"], additionalProperties: false },
    payloadSchema: { type: "object", properties: { host_id: hostId, session_id: sessionId, previous_state: { anyOf: [sessionState, { type: "null" }] }, state: sessionState, timestamp }, required: ["host_id", "session_id", "previous_state", "state", "timestamp"], additionalProperties: false },
  },
]);

export function validArguments(name, args) {
  if (!EVENT_CATALOG.some((event) => event.name === name) || !args || typeof args !== "object" || Array.isArray(args)) return false;
  const allowed = name === "job.state.changed" ? ["host_id", "session_id", "job_id"] : ["host_id", "session_id"];
  if (Object.keys(args).some((key) => !allowed.includes(key))) return false;
  return validateHostId(args.host_id) && validateSessionId(args.session_id)
    && (args.job_id === undefined || (typeof args.job_id === "string" && /^[A-Za-z0-9._:-]{1,128}$/.test(args.job_id)));
}

export function matches(subscription, transition) {
  return subscription.name === transition.name
    && subscription.host_id === transition.host_id
    && subscription.session_id === transition.session_id
    && subscription.instance_key === transition.instance_key
    && (!subscription.job_id || subscription.job_id === transition.data.job_id);
}
