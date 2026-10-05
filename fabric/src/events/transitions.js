import { validateHostId, validateSessionId } from "../protocol.js";
import { currentSession, sessionIdentity } from "./service.js";
import { recordTransition, durableReady } from "./repository.js";
import { sweepEventOutbox } from "./outbox.js";

const STATES = new Set(["starting", "active", "stopping", "stopped", "crashed", "failed"]);
const JOB_STATES = new Set(["running", "completed", "failed", "stopped", "unknown"]);

function toolView(payload) {
  const text = payload?.result?.content?.find((item) => item?.type === "text")?.text;
  if (typeof text !== "string" || text.length > 131_072) return null;
  try { return JSON.parse(text); } catch { return null; }
}

export async function observeHostResponse(env, hostId, request, payload) {
  if (!await durableReady(env) || payload?.error || request?.method !== "tools/call") return;
  const tool = request.params?.name;
  const args = request.params?.arguments;
  const sessionId = args?.session_id;
  if (!validateHostId(hostId) || !validateSessionId(sessionId)) return;
  const view = toolView(payload);
  if (!view) return;
  let instanceKey;
  const transitions = [];
  if (tool === "session_info") {
    instanceKey = sessionIdentity(view, hostId, sessionId);
    if (!instanceKey || !STATES.has(view.status)) return;
    const current = await currentSession(env, hostId, sessionId);
    if (!current || current.instance_key !== instanceKey) return;
    transitions.push({ name: "session.state.changed", data: { host_id: hostId, session_id: sessionId, state: view.status, timestamp: new Date().toISOString() } });
  } else if (tool === "job_list" || tool === "poll_job" || tool === "stop_job") {
    const session = await currentSession(env, hostId, sessionId);
    if (!session) return;
    instanceKey = session.instance_key;
    const jobs = tool === "job_list" ? view.jobs : [view];
    if (!Array.isArray(jobs) || jobs.length > 128) return;
    for (const job of jobs) {
      const jobId = job?.job_id ?? args.job_id;
      const status = job?.status ?? (Number.isInteger(job?.exit_code) ? "completed" : null);
      if (typeof jobId !== "string" || !/^[A-Za-z0-9._:-]{1,128}$/.test(jobId) || !JOB_STATES.has(status)) continue;
      transitions.push({ name: "job.state.changed", data: { host_id: hostId, session_id: sessionId, job_id: jobId, state: status, timestamp: new Date().toISOString() } });
    }
  } else return;
  for (const transition of transitions) {
    await recordTransition(env.OBSERVATION_DB, { ...transition, host_id: hostId, session_id: sessionId, instance_key: instanceKey });
  }
  if (transitions.length) await sweepEventOutbox(env);
}
