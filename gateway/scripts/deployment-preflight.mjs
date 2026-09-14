#!/usr/bin/env node

import { readFile } from "node:fs/promises";
import { resolve } from "node:path";
import { pathToFileURL } from "node:url";

function usage() {
  return "usage: node scripts/deployment-preflight.mjs --hostname <host> [--route <host>/* | --custom-domain <host>] [--config <path>]";
}

function parseArgs(argv) {
  const options = {
    config: "wrangler.toml",
    hostname: undefined,
    route: undefined,
    customDomain: undefined,
  };

  for (let index = 0; index < argv.length; index += 1) {
    const flag = argv[index];
    const value = argv[index + 1];
    if (!["--config", "--hostname", "--route", "--custom-domain"].includes(flag) || value === undefined) {
      throw new Error(usage());
    }
    index += 1;
    if (flag === "--config") options.config = value;
    if (flag === "--hostname") options.hostname = value;
    if (flag === "--route") options.route = value;
    if (flag === "--custom-domain") options.customDomain = value;
  }

  if (!options.hostname || (options.route !== undefined) === (options.customDomain !== undefined)) {
    throw new Error(usage());
  }
  return options;
}

function normalizeHostname(value) {
  if (!value || value.length > 253 || value.includes("/") || value.includes(":") || value.includes("*")) {
    return undefined;
  }
  const hostname = value.toLowerCase().replace(/\.$/, "");
  if (hostname.length === 0 || hostname.split(".").some((label) => !/^[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?$/.test(label))) {
    return undefined;
  }
  return hostname;
}

function targetResult(kind, expectedHostname, value) {
  if (value === undefined) {
    return { kind, status: "target_missing" };
  }
  const expected = kind === "route" ? `${expectedHostname}/*` : expectedHostname;
  return {
    kind,
    status: value.toLowerCase() === expected ? "target_present" : "target_mismatch",
  };
}

export function evaluateDeploymentPreflight({ configText, hostname, route, customDomain }) {
  const expectedHostname = normalizeHostname(hostname);
  if (!expectedHostname) {
    return {
      status: "invalid_hostname",
      target: { status: "not_checked" },
      remote: { status: "not_checked" },
    };
  }

  const workersDev = /^\s*workers_dev\s*=\s*(true|false)\s*$/m.exec(configText)?.[1];
  if (workersDev !== "false") {
    return {
      status: "invalid_config",
      workers_dev: workersDev ?? "missing",
      target: { status: "not_checked" },
      remote: { status: "not_checked" },
    };
  }

  const kind = route !== undefined ? "route" : "custom_domain";
  const target = targetResult(kind, expectedHostname, route ?? customDomain);
  if (target.status !== "target_present") {
    return {
      status: target.status,
      workers_dev: false,
      target,
      remote: { status: "not_checked" },
    };
  }

  return {
    status: "remote_unknown",
    workers_dev: false,
    target,
    remote: {
      status: "unknown",
      reason: "remote_target_not_checked_without_cloudflare_credentials",
    },
  };
}

export async function run(argv) {
  const options = parseArgs(argv);
  const configText = await readFile(options.config, "utf8");
  const result = evaluateDeploymentPreflight({
    configText,
    hostname: options.hostname,
    route: options.route,
    customDomain: options.customDomain,
  });
  process.stdout.write(`${JSON.stringify(result)}\n`);
  return result.status === "remote_unknown" ? 1 : result.status === "target_present" ? 0 : 1;
}

if (process.argv[1] && import.meta.url === pathToFileURL(resolve(process.argv[1])).href) {
  try {
    process.exitCode = await run(process.argv.slice(2));
  } catch (error) {
    process.stderr.write(`${error instanceof Error ? error.message : "preflight failed"}\n`);
    process.exitCode = 2;
  }
}
