import test from "node:test";
import assert from "node:assert/strict";
import { SignJWT, generateKeyPair } from "jose";

import {
  createOAuthAuthenticator,
  createPublisherAuthenticator,
  hasRequiredScope,
  oauthConfigFromEnv,
  requiredScopeForMcpRequest
} from "../lib/oauth.js";

const NOW = 1_785_283_200;
const ISSUER = "https://auth.example.test";
const AUDIENCE = "https://artifacts.example.test/mcp";
const config = oauthConfigFromEnv({
  MCP_OAUTH_ISSUER: ISSUER,
  MCP_OAUTH_AUDIENCE: AUDIENCE,
  MCP_OAUTH_JWKS_URL: "https://auth.example.test/jwks"
});
const { privateKey, publicKey } = await generateKeyPair("RS256");
const otherKeys = await generateKeyPair("RS256");

async function token(claims = {}, key = privateKey) {
  const payload = {
    sub: "ci-publisher",
    client_id: "ci-publisher",
    client_name: "CI publisher",
    org: "Acme",
    role: "author",
    scope: "artifacts:read artifacts:publish artifacts:review",
    ...claims
  };
  return new SignJWT(payload)
    .setProtectedHeader({ alg: "RS256", kid: "current", typ: "at+jwt" })
    .setIssuer(payload.iss ?? ISSUER)
    .setAudience(payload.aud ?? AUDIENCE)
    .setIssuedAt(payload.iat ?? NOW)
    .setNotBefore(payload.nbf ?? NOW)
    .setExpirationTime(payload.exp ?? NOW + 600)
    .sign(key);
}

function request(accessToken) {
  return { headers: { authorization: `Bearer ${accessToken}` } };
}

test("OAuth service tokens map verified claims and explicit scopes", async () => {
  const authenticate = createOAuthAuthenticator({
    config,
    jwks: publicKey,
    now: () => NOW
  });
  const identity = await authenticate(request(await token()));
  assert.equal(identity.ok, true);
  assert.equal(identity.clientId, "ci-publisher");
  assert.equal(identity.org, "acme");
  assert.equal(identity.label, "CI publisher");
  assert.equal(identity.role, "author");
  assert.equal(identity.authType, "oauth");
  assert.deepEqual(
    [...identity.scopes],
    ["artifacts:read", "artifacts:publish", "artifacts:review"]
  );
});

test("OAuth rejects issuer, audience, signature, time, and lifetime failures", async () => {
  const authenticate = createOAuthAuthenticator({
    config,
    jwks: publicKey,
    now: () => NOW
  });
  const rejected = [
    await token({ iss: "https://attacker.example" }),
    await token({ aud: "https://other.example/mcp" }),
    await token({}, otherKeys.privateKey),
    await token({ exp: NOW - 31 }),
    await token({ nbf: NOW + 31 }),
    await token({ exp: NOW + 3601 })
  ];
  for (const accessToken of rejected) {
    assert.deepEqual(await authenticate(request(accessToken)), { ok: false });
  }
});

test("OAuth configuration is optional, complete, asymmetric, and API-key compatible", async () => {
  const defaults = oauthConfigFromEnv({});
  assert.equal(defaults.enabled, false);
  assert.equal(defaults.apiKeysEnabled, true);
  assert.throws(
    () => oauthConfigFromEnv({ MCP_OAUTH_ISSUER: ISSUER }),
    /requires MCP_OAUTH_ISSUER/
  );
  assert.throws(
    () => oauthConfigFromEnv({
      MCP_OAUTH_ISSUER: ISSUER,
      MCP_OAUTH_AUDIENCE: AUDIENCE,
      MCP_OAUTH_JWKS_URL: "https://auth.example.test/jwks",
      MCP_OAUTH_ALLOWED_ALGS: "HS256"
    }),
    /asymmetric JWS/
  );

  const authenticate = createPublisherAuthenticator({
    config,
    checkApiKey: () => ({
      ok: true,
      clientId: "legacy",
      org: "acme",
      role: "author"
    }),
    authenticateOAuth: async () => ({ ok: false })
  });
  const legacy = await authenticate({ headers: { "x-api-key": "legacy" } });
  assert.equal(legacy.authType, "api_key");
  assert.equal(legacy.scopes, null);
});

test("OAuth scope mapping separates read, publish, review, visibility, and delete", () => {
  const cases = [
    ["read_artifact", "artifacts:read"],
    ["publish_artifact", "artifacts:publish"],
    ["submit_feedback", "artifacts:review"],
    ["set_visibility", "artifacts:visibility"],
    ["delete_artifact", "artifacts:delete"]
  ];
  for (const [name, expected] of cases) {
    const required = requiredScopeForMcpRequest({
      method: "tools/call",
      params: { name }
    });
    assert.equal(required, expected);
    assert.equal(
      hasRequiredScope({ authType: "oauth", scopes: new Set([expected]) }, required),
      true
    );
    assert.equal(
      hasRequiredScope({ authType: "oauth", scopes: new Set() }, required),
      false
    );
  }
  assert.equal(
    requiredScopeForMcpRequest({ method: "resources/read", params: {} }),
    "artifacts:read"
  );
  assert.equal(requiredScopeForMcpRequest({ method: "server/discover" }), null);
});
