// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Neil Blackman
import { createRemoteJWKSet, jwtVerify } from "jose";

export const OAUTH_EXTENSION = "io.modelcontextprotocol/oauth-client-credentials";
export const OAUTH_SCOPES = Object.freeze([
  "artifacts:read",
  "artifacts:publish",
  "artifacts:review",
  "artifacts:visibility",
  "artifacts:delete",
  "audit:read",
  "audit:export",
  "audit:global"
]);

const DEFAULT_ALGORITHMS = Object.freeze(["RS256"]);
const SUPPORTED_ALGORITHMS = new Set([
  "RS256", "RS384", "RS512", "PS256", "PS384", "PS512",
  "ES256", "ES384", "EdDSA", "Ed25519"
]);

function positiveInteger(raw, fallback, name) {
  if (raw == null || String(raw).trim() === "") return fallback;
  const parsed = Number(raw);
  if (!Number.isSafeInteger(parsed) || parsed <= 0) {
    throw new Error(`${name} must be a positive integer`);
  }
  return parsed;
}

function exactSwitch(raw, fallback, name) {
  if (raw == null || String(raw).trim() === "") return fallback;
  if (raw === "1") return true;
  if (raw === "0") return false;
  throw new Error(`${name} must be "0" or "1"`);
}

function absoluteHttpUrl(raw, name) {
  let parsed;
  try {
    parsed = new URL(raw);
  } catch {
    throw new Error(`${name} must be an absolute http(s) URL`);
  }
  if (!["http:", "https:"].includes(parsed.protocol) || parsed.hash) {
    throw new Error(`${name} must be an absolute http(s) URL without a fragment`);
  }
  return String(raw).trim();
}

export function oauthConfigFromEnv(env = process.env) {
  const issuer = String(env.MCP_OAUTH_ISSUER || "").trim();
  const audience = String(env.MCP_OAUTH_AUDIENCE || "").trim();
  const jwksUrl = String(env.MCP_OAUTH_JWKS_URL || "").trim();
  const configured = [issuer, audience, jwksUrl].filter(Boolean).length;
  if (configured !== 0 && configured !== 3) {
    throw new Error(
      "MCP OAuth requires MCP_OAUTH_ISSUER, MCP_OAUTH_AUDIENCE, and MCP_OAUTH_JWKS_URL together"
    );
  }
  const algorithms = String(env.MCP_OAUTH_ALLOWED_ALGS || DEFAULT_ALGORITHMS.join(","))
    .split(",")
    .map((value) => value.trim())
    .filter(Boolean);
  if (!algorithms.length || algorithms.some((algorithm) => !SUPPORTED_ALGORITHMS.has(algorithm))) {
    throw new Error(
      "MCP_OAUTH_ALLOWED_ALGS must contain only supported asymmetric JWS algorithms"
    );
  }
  return Object.freeze({
    enabled: configured === 3,
    issuer: configured ? absoluteHttpUrl(issuer, "MCP_OAUTH_ISSUER") : "",
    audience,
    jwksUrl: configured ? absoluteHttpUrl(jwksUrl, "MCP_OAUTH_JWKS_URL") : "",
    algorithms: Object.freeze([...new Set(algorithms)]),
    clockTolerance: positiveInteger(
      env.MCP_OAUTH_CLOCK_TOLERANCE_S,
      30,
      "MCP_OAUTH_CLOCK_TOLERANCE_S"
    ),
    maxTokenLifetime: positiveInteger(
      env.MCP_OAUTH_MAX_TOKEN_LIFETIME_S,
      3600,
      "MCP_OAUTH_MAX_TOKEN_LIFETIME_S"
    ),
    apiKeysEnabled: exactSwitch(env.MCP_API_KEYS_ENABLED, true, "MCP_API_KEYS_ENABLED")
  });
}

function bearerToken(req) {
  const raw = Array.isArray(req?.headers?.authorization)
    ? req.headers.authorization[0]
    : req?.headers?.authorization;
  const match = String(raw || "").match(/^\s*Bearer\s+(.+?)\s*$/i);
  return match ? match[1].trim() : "";
}

function scopesFromPayload(payload) {
  if (payload.scope !== undefined) {
    if (typeof payload.scope !== "string") throw new Error("invalid OAuth scope claim");
    return new Set(payload.scope.split(/\s+/).filter(Boolean));
  }
  if (payload.scp === undefined) return new Set();
  if (typeof payload.scp === "string") return new Set(payload.scp.split(/\s+/).filter(Boolean));
  if (Array.isArray(payload.scp) && payload.scp.every((scope) => typeof scope === "string")) {
    return new Set(payload.scp);
  }
  throw new Error("invalid OAuth scp claim");
}

function identityFromPayload(payload, config, nowSeconds) {
  if (!Number.isSafeInteger(payload.iat) || !Number.isSafeInteger(payload.exp)) {
    throw new Error("OAuth access token requires integer iat and exp claims");
  }
  if (payload.iat > nowSeconds + config.clockTolerance) {
    throw new Error("OAuth access token is not yet valid");
  }
  if (payload.nbf !== undefined) {
    if (!Number.isSafeInteger(payload.nbf)) throw new Error("invalid OAuth nbf claim");
    if (payload.nbf > nowSeconds + config.clockTolerance) {
      throw new Error("OAuth access token is not yet valid");
    }
  }
  if (payload.exp <= nowSeconds - config.clockTolerance) {
    throw new Error("OAuth access token is expired");
  }
  if (payload.exp <= payload.iat || payload.exp - payload.iat > config.maxTokenLifetime) {
    throw new Error("OAuth access token lifetime exceeds policy");
  }
  if (
    payload.client_id !== undefined
    && payload.sub !== undefined
    && payload.client_id !== payload.sub
  ) {
    throw new Error("OAuth client_id and sub claims disagree");
  }
  const clientId = String(payload.client_id || payload.sub || "").trim();
  const org = String(payload.org || "").trim().toLowerCase();
  if (!clientId || !org) throw new Error("OAuth access token requires client identity and org");
  const role = payload.role === undefined ? "author" : payload.role;
  if (!["reader", "author", "collaborator"].includes(role)) {
    throw new Error("invalid OAuth role claim");
  }
  return {
    ok: true,
    clientId,
    org,
    label: String(payload.client_name || clientId).trim().slice(0, 120),
    role,
    ownerEmail: null,
    scopes: scopesFromPayload(payload),
    authType: "oauth"
  };
}

export function createOAuthAuthenticator({
  config,
  verifyJwt = jwtVerify,
  jwks = config?.enabled ? createRemoteJWKSet(new URL(config.jwksUrl), {
    timeoutDuration: 5000,
    cooldownDuration: 30000,
    cacheMaxAge: 600000
  }) : null,
  now = () => Math.floor(Date.now() / 1000)
}) {
  return async function authenticateOAuth(req) {
    if (!config?.enabled) return { ok: false };
    const token = bearerToken(req);
    if (!token) return { ok: false };
    try {
      const nowSeconds = now();
      const { payload } = await verifyJwt(token, jwks, {
        issuer: config.issuer,
        audience: config.audience,
        algorithms: config.algorithms,
        clockTolerance: config.clockTolerance,
        currentDate: new Date(nowSeconds * 1000)
      });
      return identityFromPayload(payload, config, nowSeconds);
    } catch {
      return { ok: false };
    }
  };
}

export function createPublisherAuthenticator({
  config,
  checkApiKey,
  authenticateOAuth = createOAuthAuthenticator({ config })
}) {
  if (!config.apiKeysEnabled && !config.enabled) {
    throw new Error(
      "MCP_API_KEYS_ENABLED=0 requires a complete MCP OAuth configuration; refusing to start"
    );
  }
  return async function authenticatePublisher(req) {
    if (config.apiKeysEnabled) {
      const apiKey = await checkApiKey(req);
      if (apiKey?.ok) {
        return { ...apiKey, scopes: null, authType: "api_key" };
      }
    }
    return authenticateOAuth(req);
  };
}

export function requiredScopeForMcpRequest(payload) {
  const method = payload?.method;
  if (["resources/list", "resources/templates/list", "resources/read"].includes(method)) {
    return "artifacts:read";
  }
  if (method === "tasks/get") return "artifacts:read";
  if (method === "tasks/update" || method === "tasks/cancel") return "artifacts:publish";
  if (method !== "tools/call") return null;
  const name = payload?.params?.name;
  if ([
    "list_artifacts", "read_artifact", "list_categories", "list_revisions",
    "list_shares", "artifact_stats"
  ].includes(name)) return "artifacts:read";
  if ([
    "publish_artifact", "publish_bundle", "update_artifact", "patch_artifact",
    "set_category", "create_category", "delete_category", "restore_artifact",
    "regenerate_artifact_preview"
  ].includes(name)) return "artifacts:publish";
  if ([
    "list_feedback", "resolve_feedback", "reopen_feedback", "submit_feedback"
  ].includes(name)) return "artifacts:review";
  if (["set_visibility", "create_share", "revoke_share"].includes(name)) {
    return "artifacts:visibility";
  }
  if (name === "delete_artifact") return "artifacts:delete";
  return null;
}

export function hasRequiredScope(auth, required) {
  if (!required || auth?.authType !== "oauth") return true;
  return auth.scopes instanceof Set && auth.scopes.has(required);
}

export function protectedResourceMetadata(config, publicBase) {
  return {
    resource: `${String(publicBase).replace(/\/$/, "")}/mcp`,
    authorization_servers: [config.issuer],
    bearer_methods_supported: ["header"],
    scopes_supported: OAUTH_SCOPES
  };
}

export function resourceMetadataUrl(publicBase) {
  const parsed = new URL(publicBase);
  parsed.pathname = "/.well-known/oauth-protected-resource";
  parsed.search = "";
  parsed.hash = "";
  return parsed.toString();
}
