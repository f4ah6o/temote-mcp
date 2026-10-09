import { authorizeDashboard } from "../access.js";
import { handleDashboardApi } from "./api.js";

export const DASHBOARD_SECURITY_HEADERS = Object.freeze({
  "cache-control": "no-store",
  "content-security-policy": [
    "default-src 'self'",
    "script-src 'self'",
    "style-src 'self'",
    "connect-src 'self'",
    "img-src 'self' data:",
    "frame-ancestors 'none'",
    "base-uri 'none'",
    "form-action 'none'",
    "object-src 'none'",
  ].join("; "),
  "cross-origin-resource-policy": "same-origin",
  "permissions-policy": "camera=(), microphone=(), geolocation=()",
  "referrer-policy": "no-referrer",
  "x-content-type-options": "nosniff",
  "x-frame-options": "DENY",
});

const STATIC_ASSETS = new Map([
  ["/dash/", "/dash/index.html"],
  ["/dash/app.js", "/dash/app.js"],
  ["/dash/styles.css", "/dash/styles.css"],
]);

export function isDashboardPathname(pathname) {
  if (typeof pathname !== "string") return false;
  // Static asset routers and URL parsers can canonicalize escaped separators,
  // backslashes, repeated slashes, and case. Route those aliases through the
  // Access guard too, while keeping asset selection below strict and literal.
  const partiallyDecoded = pathname.replace(/%([0-9a-f]{2})/gi, (_match, hex) => (
    String.fromCharCode(Number.parseInt(hex, 16))
  ));
  const normalized = partiallyDecoded
    .replaceAll("\\", "/")
    .replace(/\/{2,}/g, "/")
    .toLowerCase();
  return normalized === "/dash"
    || normalized.startsWith("/dash/")
    || normalized.startsWith("/dash%");
}

export function dashboardResponse(response) {
  const headers = new Headers(response.headers);
  for (const name of [...headers.keys()]) {
    if (name.toLowerCase().startsWith("access-control-")) headers.delete(name);
  }
  for (const [name, value] of Object.entries(DASHBOARD_SECURITY_HEADERS)) {
    headers.set(name, value);
  }
  return new Response(response.body, {
    status: response.status,
    statusText: response.statusText,
    headers,
  });
}

export function dashboardJson(value, status = 200, extraHeaders = {}) {
  return dashboardResponse(new Response(JSON.stringify(value), {
    status,
    headers: { "content-type": "application/json; charset=utf-8", ...extraHeaders },
  }));
}

export async function handleDashboardRequest(request, env) {
  try {
    const identity = await authorizeDashboard(request, env);
    if (!identity) return dashboardJson({ error_code: "access_unauthorized" }, 401);
    const url = new URL(request.url);
    if (identity.auth_mode === "browser_owner") {
      // The static shell carries no owner data and can only call the guarded
      // dashboard API below. Keep it available while denying every data route
      // until the shared replica has an owner/root namespace proof.
      if (request.method !== "GET") {
        return dashboardJson({ error_code: "browser_data_surface_unavailable" }, 403);
      }
      if (url.pathname === "/dash") {
        return dashboardResponse(new Response(null, {
          status: 308,
          headers: { location: "/dash/" },
        }));
      }
      const browserAssetPath = STATIC_ASSETS.get(url.pathname);
      if (browserAssetPath) return await serveDashboardAsset(browserAssetPath, url, env);
      return dashboardJson({ error_code: "browser_data_surface_unavailable" }, 403);
    }

    if (request.method !== "GET") {
      return dashboardJson({ error_code: "method_not_allowed" }, 405, { allow: "GET" });
    }

    if (url.pathname === "/dash") {
      return dashboardResponse(new Response(null, {
        status: 308,
        headers: { location: "/dash/" },
      }));
    }

    if (url.pathname === "/dash/api" || url.pathname.startsWith("/dash/api/")) {
      return dashboardResponse(await handleDashboardApi(request, env));
    }

    const assetPath = STATIC_ASSETS.get(url.pathname);
    if (assetPath) {
      return await serveDashboardAsset(assetPath, url, env);
    }

    return dashboardJson({ error_code: "not_found" }, 404);
  } catch {
    // Keep all dashboard exceptions on the same no-store, same-origin response
    // path. Do not log error bodies that may contain host-provided detail.
    return dashboardJson({ error_code: "internal_error" }, 500);
  }
}

async function serveDashboardAsset(assetPath, url, env) {
  if (!env?.ASSETS || typeof env.ASSETS.fetch !== "function") {
    return dashboardJson({ error_code: "dashboard_assets_unavailable" }, 503);
  }
  const assetUrl = new URL(assetPath, url.origin);
  const asset = await env.ASSETS.fetch(new Request(assetUrl, { method: "GET" }));
  if (!asset.ok) return dashboardJson({ error_code: "dashboard_asset_unavailable" }, 503);
  return dashboardResponse(asset);
}
