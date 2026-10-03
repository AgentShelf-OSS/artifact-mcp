import test from "node:test";
import assert from "node:assert/strict";
import { createRemoteJWKSet, exportJWK, SignJWT, generateKeyPair } from "jose";
import http from "node:http";
import https from "node:https";
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";

import {
  OAUTH_SCOPES,
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

test("OAuth URLs require HTTPS unless explicitly limited to loopback development", () => {
  const base = {
    MCP_OAUTH_ISSUER: ISSUER,
    MCP_OAUTH_AUDIENCE: AUDIENCE,
    MCP_OAUTH_JWKS_URL: "https://auth.example.test/jwks"
  };
  const accepted = [
    "http://localhost/issuer",
    "http://127.0.0.1/jwks",
    "http://127.1/jwks",
    "http://0x7f000001/jwks",
    "http://[::1]/jwks",
    "http://[0:0:0:0:0:0:0:1]/jwks"
  ];
  for (const jwksUrl of accepted) {
    const configWithOptIn = oauthConfigFromEnv({
      ...base,
      MCP_OAUTH_JWKS_URL: jwksUrl,
      MCP_OAUTH_ALLOW_LOOPBACK_HTTP: "1"
    });
    assert.equal(configWithOptIn.jwksUrl, jwksUrl);
    assert.equal(configWithOptIn.allowLoopbackHttp, true);
    assert.throws(
      () => oauthConfigFromEnv({ ...base, MCP_OAUTH_JWKS_URL: jwksUrl }),
      /MCP_OAUTH_JWKS_URL must be an absolute HTTPS URL/
    );
  }
  const loopbackIssuer = oauthConfigFromEnv({
    ...base,
    MCP_OAUTH_ISSUER: "http://localhost/issuer",
    MCP_OAUTH_JWKS_URL: "http://127.0.0.1/jwks",
    MCP_OAUTH_ALLOW_LOOPBACK_HTTP: "1"
  });
  assert.equal(loopbackIssuer.issuer, "http://localhost/issuer");
  assert.equal(loopbackIssuer.jwksUrl, "http://127.0.0.1/jwks");
  assert.throws(
    () => oauthConfigFromEnv({
      ...base,
      MCP_OAUTH_ISSUER: "http://auth.example.test/issuer",
      MCP_OAUTH_ALLOW_LOOPBACK_HTTP: "1"
    }),
    /MCP_OAUTH_ISSUER must be an absolute HTTPS URL/
  );

  const rejected = [
    "http://localhost./jwks",
    "http://localhost.example/jwks",
    "http://127.0.0.1.example/jwks",
    "http://192.168.1.10/jwks",
    "http://10.0.0.1/jwks",
    "http://[::ffff:127.0.0.1]/jwks"
  ];
  for (const jwksUrl of rejected) {
    assert.throws(
      () => oauthConfigFromEnv({
        ...base,
        MCP_OAUTH_JWKS_URL: jwksUrl,
        MCP_OAUTH_ALLOW_LOOPBACK_HTTP: "1"
      }),
      /MCP_OAUTH_JWKS_URL must be an absolute HTTPS URL/
    );
  }
});

test("OAuth URL errors reject credentials and fragments without echoing URL contents", () => {
  const base = {
    MCP_OAUTH_ISSUER: ISSUER,
    MCP_OAUTH_AUDIENCE: AUDIENCE,
    MCP_OAUTH_JWKS_URL: "https://auth.example.test/jwks"
  };
  for (const [setting, value] of [
    ["MCP_OAUTH_ISSUER", "https://user:secret@auth.example.test/issuer"],
    ["MCP_OAUTH_ISSUER", "https://@auth.example.test/issuer"],
    ["MCP_OAUTH_ISSUER", "https://auth.example.test/issuer?token=top-secret#"],
    ["MCP_OAUTH_JWKS_URL", "https://auth.example.test/jwks#fragment"],
    ["MCP_OAUTH_JWKS_URL", "https://auth.example.test/jwks#"]
  ]) {
    assert.throws(() => oauthConfigFromEnv({ ...base, [setting]: value }), (error) => {
      assert.match(error.message, new RegExp(`${setting} must be`));
      assert.equal(error.message.includes(value), false);
      return true;
    });
  }
  assert.throws(
    () => oauthConfigFromEnv({
      ...base,
      MCP_OAUTH_ALLOW_LOOPBACK_HTTP: "yes"
    }),
    /MCP_OAUTH_ALLOW_LOOPBACK_HTTP must be "0" or "1"/
  );
  assert.throws(
    () => oauthConfigFromEnv({
      ...base,
      MCP_OAUTH_ALLOW_LOOPBACK_HTTP: " 1 "
    }),
    /MCP_OAUTH_ALLOW_LOOPBACK_HTTP must be "0" or "1"/
  );
  assert.equal(
    oauthConfigFromEnv({ MCP_OAUTH_ALLOW_LOOPBACK_HTTP: "yes" }).enabled,
    false
  );
});

test("JOSE rejects JWKS redirects without requesting the plaintext target", async () => {
  let targetRequests = 0;
  const target = http.createServer((_request, response) => {
    targetRequests += 1;
    response.writeHead(200, { "content-type": "application/json" });
    response.end(JSON.stringify({ keys: [] }));
  });
  const redirect = http.createServer((_request, response) => {
    response.writeHead(302, { location: `http://127.0.0.1:${target.address().port}/jwks` });
    response.end();
  });
  await new Promise((resolve) => target.listen(0, "127.0.0.1", resolve));
  await new Promise((resolve) => redirect.listen(0, "127.0.0.1", resolve));
  try {
    const jwks = createRemoteJWKSet(new URL(`http://127.0.0.1:${redirect.address().port}/jwks`));
    await assert.rejects(
      () => jwks({ alg: "RS256", kid: "missing" }, "synthetic-token"),
      /200 OK/
    );
    assert.equal(targetRequests, 0);
  } finally {
    await Promise.all([
      new Promise((resolve) => redirect.close(resolve)),
      new Promise((resolve) => target.close(resolve))
    ]);
  }
});

test("HTTPS JWKS retrieval never follows a redirect to plaintext HTTP", async () => {
  const fixtureDirectory = fileURLToPath(new URL("./fixtures/oauth-https/", import.meta.url));
  const tls = {
    cert: readFileSync(`${fixtureDirectory}localhost-cert.pem`),
    key: readFileSync(`${fixtureDirectory}localhost-key.pem`),
    ca: readFileSync(`${fixtureDirectory}ca-cert.pem`)
  };
  let targetRequests = 0;
  const target = http.createServer((_request, response) => {
    targetRequests += 1;
    response.writeHead(200, { "content-type": "application/json" });
    response.end(JSON.stringify({ keys: [] }));
  });
  const redirect = https.createServer(tls, (_request, response) => {
    response.writeHead(302, { location: `http://127.0.0.1:${target.address().port}/jwks` });
    response.end();
  });
  await new Promise((resolve) => target.listen(0, "127.0.0.1", resolve));
  await new Promise((resolve) => redirect.listen(0, "127.0.0.1", resolve));
  try {
    const agent = new https.Agent({ ca: tls.ca });
    const jwks = createRemoteJWKSet(
      new URL(`https://127.0.0.1:${redirect.address().port}/jwks`),
      { agent }
    );
    await assert.rejects(
      () => jwks({ alg: "RS256", kid: "missing" }, "synthetic-token"),
      /200 OK/
    );
    assert.equal(targetRequests, 0);
    agent.destroy();
  } finally {
    await Promise.all([
      new Promise((resolve) => redirect.close(resolve)),
      new Promise((resolve) => target.close(resolve))
    ]);
  }
});

test("HTTPS JWKS retrieval verifies a token and uses the JOSE cache", async () => {
  const fixtureDirectory = fileURLToPath(new URL("./fixtures/oauth-https/", import.meta.url));
  const tls = {
    cert: readFileSync(`${fixtureDirectory}localhost-cert.pem`),
    key: readFileSync(`${fixtureDirectory}localhost-key.pem`),
    ca: readFileSync(`${fixtureDirectory}ca-cert.pem`)
  };
  const jwk = await exportJWK(publicKey);
  jwk.alg = "RS256";
  jwk.kid = "current";
  let jwksRequests = 0;
  const server = https.createServer(tls, (_request, response) => {
    jwksRequests += 1;
    response.writeHead(200, { "content-type": "application/json" });
    response.end(JSON.stringify({ keys: [jwk] }));
  });
  await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
  const agent = new https.Agent({ ca: tls.ca });
  try {
    const jwksUrl = `https://127.0.0.1:${server.address().port}/jwks`;
    const secureConfig = oauthConfigFromEnv({
      MCP_OAUTH_ISSUER: ISSUER,
      MCP_OAUTH_AUDIENCE: AUDIENCE,
      MCP_OAUTH_JWKS_URL: jwksUrl
    });
    const authenticate = createOAuthAuthenticator({
      config: secureConfig,
      jwks: createRemoteJWKSet(new URL(jwksUrl), { agent }),
      now: () => NOW
    });
    const accessToken = await token();
    assert.equal((await authenticate(request(accessToken))).ok, true);
    assert.equal((await authenticate(request(accessToken))).ok, true);
    assert.equal(jwksRequests, 1);
  } finally {
    agent.destroy();
    await new Promise((resolve) => server.close(resolve));
  }
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

test("OAuth metadata advertises the explicit audit scopes", () => {
  assert.deepEqual(
    OAUTH_SCOPES.filter((scope) => scope.startsWith("audit:")),
    ["audit:read", "audit:export", "audit:global"]
  );
});
