import assert from "node:assert/strict";
import {
  createHash,
  generateKeyPairSync,
  randomBytes,
  randomUUID,
} from "node:crypto";
import test from "node:test";

import {
  fakeAuthorizationInternals,
  startFakeAuthorizationServer,
} from "./fake-authorization-server.mjs";

const { base64url, decodeJson, jwkThumbprint, signJwt } = fakeAuthorizationInternals;

function dpopProof(privateKey, publicKey, method, targetUrl) {
  const now = Math.floor(Date.now() / 1000);
  const jwk = publicKey.export({ format: "jwk" });
  return signJwt(
    { typ: "dpop+jwt", alg: "ES256", jwk },
    { htm: method, htu: targetUrl, iat: now, jti: randomUUID() },
    privateKey,
  );
}

async function authorizationCode(server, verifier) {
  const redirectUri = "http://127.0.0.1:49152/callback";
  const challenge = base64url(createHash("sha256").update(verifier).digest());
  const authorize = new URL(`${server.issuer}/authorize`);
  authorize.search = new URLSearchParams({
    response_type: "code",
    client_id: server.clientId,
    redirect_uri: redirectUri,
    scope: server.scope,
    state: "test-state",
    code_challenge: challenge,
    code_challenge_method: "S256",
  });
  const response = await fetch(authorize, { redirect: "manual" });
  assert.equal(response.status, 302);
  const callback = new URL(response.headers.get("location"));
  assert.equal(callback.searchParams.get("state"), "test-state");
  return { code: callback.searchParams.get("code"), redirectUri };
}

test("issues a DPoP-bound token for an S256 authorization code", async (context) => {
  const server = await startFakeAuthorizationServer();
  context.after(() => server.close());

  const metadata = await fetch(`${server.issuer}/.well-known/oauth-authorization-server`).then((response) => response.json());
  assert.equal(metadata.issuer, server.issuer);
  assert.deepEqual(metadata.code_challenge_methods_supported, ["S256"]);
  assert.deepEqual(metadata.dpop_signing_alg_values_supported, ["ES256"]);

  const verifier = base64url(randomBytes(32));
  const { code, redirectUri } = await authorizationCode(server, verifier);
  const proofKeys = generateKeyPairSync("ec", { namedCurve: "P-256" });
  const response = await fetch(`${server.issuer}/token`, {
    method: "POST",
    headers: {
      "content-type": "application/x-www-form-urlencoded",
      dpop: dpopProof(proofKeys.privateKey, proofKeys.publicKey, "POST", `${server.issuer}/token`),
    },
    body: new URLSearchParams({
      grant_type: "authorization_code",
      client_id: server.clientId,
      redirect_uri: redirectUri,
      code,
      code_verifier: verifier,
    }),
  });
  assert.equal(response.status, 200);
  const token = await response.json();
  assert.equal(token.token_type, "DPoP");
  assert.equal(token.scope, server.scope);

  const [header, claims] = token.access_token.split(".").slice(0, 2).map(decodeJson);
  assert.equal(header.alg, "ES256");
  assert.equal(claims.iss, server.issuer);
  assert.equal(claims.aud, server.audience);
  assert.equal(claims.sub, "test-user");
  assert.equal(
    claims.cnf.jkt,
    jwkThumbprint(proofKeys.publicKey.export({ format: "jwk" })),
  );

  const replay = await fetch(`${server.issuer}/token`, {
    method: "POST",
    headers: {
      "content-type": "application/x-www-form-urlencoded",
      dpop: dpopProof(proofKeys.privateKey, proofKeys.publicKey, "POST", `${server.issuer}/token`),
    },
    body: new URLSearchParams({
      grant_type: "authorization_code",
      client_id: server.clientId,
      redirect_uri: redirectUri,
      code,
      code_verifier: verifier,
    }),
  });
  assert.equal(replay.status, 400);
  assert.equal((await replay.json()).error, "invalid_grant");
});

test("rejects a wrong PKCE verifier", async (context) => {
  const server = await startFakeAuthorizationServer();
  context.after(() => server.close());
  const verifier = base64url(randomBytes(32));
  const { code, redirectUri } = await authorizationCode(server, verifier);
  const proofKeys = generateKeyPairSync("ec", { namedCurve: "P-256" });

  const response = await fetch(`${server.issuer}/token`, {
    method: "POST",
    headers: {
      "content-type": "application/x-www-form-urlencoded",
      dpop: dpopProof(proofKeys.privateKey, proofKeys.publicKey, "POST", `${server.issuer}/token`),
    },
    body: new URLSearchParams({
      grant_type: "authorization_code",
      client_id: server.clientId,
      redirect_uri: redirectUri,
      code,
      code_verifier: `${verifier}-wrong`,
    }),
  });

  assert.equal(response.status, 400);
  assert.equal((await response.json()).error, "invalid_grant");
});
