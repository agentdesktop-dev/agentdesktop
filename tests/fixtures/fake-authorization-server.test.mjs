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

function dpopProof(privateKey, publicKey, method, targetUrl, accessToken) {
  const now = Math.floor(Date.now() / 1000);
  const jwk = publicKey.export({ format: "jwk" });
  const claims = { htm: method, htu: targetUrl, iat: now, jti: randomUUID() };
  if (accessToken) {
    claims.ath = base64url(createHash("sha256").update(accessToken).digest());
  }
  return signJwt(
    { typ: "dpop+jwt", alg: "ES256", jwk },
    claims,
    privateKey,
  );
}

async function authorizationCode(server, verifier) {
  const redirectUri = "http://127.0.0.1:49152/callback";
  const challenge = base64url(createHash("sha256").update(verifier).digest());
  const authorize = new URL("authorize", server.issuer);
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

async function issueToken(server, proofKeys) {
  const verifier = base64url(randomBytes(32));
  const { code, redirectUri } = await authorizationCode(server, verifier);
  const tokenEndpoint = new URL("token", server.issuer).toString();
  const response = await fetch(tokenEndpoint, {
    method: "POST",
    headers: {
      "content-type": "application/x-www-form-urlencoded",
      dpop: dpopProof(proofKeys.privateKey, proofKeys.publicKey, "POST", tokenEndpoint),
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
  return response.json();
}

test("issues a DPoP-bound token for an S256 authorization code", async (context) => {
  const server = await startFakeAuthorizationServer();
  context.after(() => server.close());

  const metadata = await fetch(new URL(".well-known/oauth-authorization-server", server.issuer)).then((response) => response.json());
  assert.equal(metadata.issuer, server.issuer);
  assert.deepEqual(metadata.code_challenge_methods_supported, ["S256"]);
  assert.deepEqual(metadata.dpop_signing_alg_values_supported, ["ES256"]);

  const verifier = base64url(randomBytes(32));
  const { code, redirectUri } = await authorizationCode(server, verifier);
  const proofKeys = generateKeyPairSync("ec", { namedCurve: "P-256" });
  const tokenEndpoint = new URL("token", server.issuer).toString();
  const response = await fetch(tokenEndpoint, {
    method: "POST",
    headers: {
      "content-type": "application/x-www-form-urlencoded",
      dpop: dpopProof(proofKeys.privateKey, proofKeys.publicKey, "POST", tokenEndpoint),
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
  assert.equal(typeof token.refresh_token, "string");

  const [header, claims] = token.access_token.split(".").slice(0, 2).map(decodeJson);
  assert.equal(header.alg, "ES256");
  assert.equal(claims.iss, server.issuer);
  assert.equal(claims.aud, server.audience);
  assert.equal(claims.sub, "test-user");
  assert.equal(
    claims.cnf.jkt,
    jwkThumbprint(proofKeys.publicKey.export({ format: "jwk" })),
  );

  const replay = await fetch(tokenEndpoint, {
    method: "POST",
    headers: {
      "content-type": "application/x-www-form-urlencoded",
      dpop: dpopProof(proofKeys.privateKey, proofKeys.publicKey, "POST", tokenEndpoint),
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

  const refreshProof = dpopProof(
    proofKeys.privateKey,
    proofKeys.publicKey,
    "POST",
    tokenEndpoint,
  );
  const refreshed = await fetch(tokenEndpoint, {
    method: "POST",
    headers: {
      "content-type": "application/x-www-form-urlencoded",
      dpop: refreshProof,
    },
    body: new URLSearchParams({
      grant_type: "refresh_token",
      client_id: server.clientId,
      refresh_token: token.refresh_token,
    }),
  });
  assert.equal(refreshed.status, 200);
  const refreshedToken = await refreshed.json();
  assert.notEqual(refreshedToken.refresh_token, token.refresh_token);

  const refreshReplay = await fetch(tokenEndpoint, {
    method: "POST",
    headers: {
      "content-type": "application/x-www-form-urlencoded",
      dpop: dpopProof(proofKeys.privateKey, proofKeys.publicKey, "POST", tokenEndpoint),
    },
    body: new URLSearchParams({
      grant_type: "refresh_token",
      client_id: server.clientId,
      refresh_token: token.refresh_token,
    }),
  });
  assert.equal(refreshReplay.status, 400);
  assert.equal((await refreshReplay.json()).error, "invalid_grant");
});

test("issues a separately scoped administrator token", async (context) => {
  const server = await startFakeAuthorizationServer();
  context.after(() => server.close());

  const response = await fetch(new URL("admin-token", server.issuer));
  assert.equal(response.status, 200);
  const token = await response.json();
  assert.equal(token.token_type, "Bearer");
  assert.equal(token.scope, server.administratorScope);

  const [header, claims] = token.access_token.split(".").slice(0, 2).map(decodeJson);
  assert.equal(header.alg, "ES256");
  assert.equal(claims.iss, server.issuer);
  assert.equal(claims.aud, server.audience);
  assert.equal(claims.sub, "test-admin");
  assert.equal(claims.scope, server.administratorScope);
});

test("rejects a wrong PKCE verifier", async (context) => {
  const server = await startFakeAuthorizationServer();
  context.after(() => server.close());
  const verifier = base64url(randomBytes(32));
  const { code, redirectUri } = await authorizationCode(server, verifier);
  const proofKeys = generateKeyPairSync("ec", { namedCurve: "P-256" });
  const tokenEndpoint = new URL("token", server.issuer).toString();

  const response = await fetch(tokenEndpoint, {
    method: "POST",
    headers: {
      "content-type": "application/x-www-form-urlencoded",
      dpop: dpopProof(proofKeys.privateKey, proofKeys.publicKey, "POST", tokenEndpoint),
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

test("enrolls only the token-bound key and reports device revocation", async (context) => {
  const server = await startFakeAuthorizationServer();
  context.after(() => server.close());
  const proofKeys = generateKeyPairSync("ec", { namedCurve: "P-256" });
  const token = await issueToken(server, proofKeys);
  const enrollmentEndpoint = new URL("enrollments", server.issuer).toString();

  const wrongKeys = generateKeyPairSync("ec", { namedCurve: "P-256" });
  const wrong = await fetch(enrollmentEndpoint, {
    method: "POST",
    headers: {
      authorization: `DPoP ${token.access_token}`,
      dpop: dpopProof(
        wrongKeys.privateKey,
        wrongKeys.publicKey,
        "POST",
        enrollmentEndpoint,
        token.access_token,
      ),
    },
  });
  assert.equal(wrong.status, 401);
  assert.equal((await wrong.json()).error, "invalid_dpop_proof");

  const enrollmentProof = dpopProof(
    proofKeys.privateKey,
    proofKeys.publicKey,
    "POST",
    enrollmentEndpoint,
    token.access_token,
  );
  const enrollmentRequest = () => fetch(enrollmentEndpoint, {
    method: "POST",
    headers: {
      authorization: `DPoP ${token.access_token}`,
      dpop: enrollmentProof,
    },
  });
  const requested = await enrollmentRequest();
  assert.equal(requested.status, 202);
  const enrollment = await requested.json();
  assert.equal(enrollment.status, "pending");
  assert.equal(enrollment.user.sub, "test-user");

  const replayed = await enrollmentRequest();
  assert.equal(replayed.status, 401);
  assert.equal((await replayed.json()).error, "dpop_proof_replayed");
  assert.equal(server.approveEnrollment(enrollment.enrollment_id, "device-1"), true);

  const statusEndpoint = new URL(
    `enrollments/${enrollment.enrollment_id}`,
    server.issuer,
  ).toString();
  const readStatus = () => fetch(statusEndpoint, {
    headers: {
      authorization: `DPoP ${token.access_token}`,
      dpop: dpopProof(
        proofKeys.privateKey,
        proofKeys.publicKey,
        "GET",
        statusEndpoint,
        token.access_token,
      ),
    },
  });
  const approved = await readStatus();
  assert.equal(approved.status, 200);
  assert.deepEqual(await approved.json(), {
    ...enrollment,
    status: "approved",
    device_id: "device-1",
    device_status: "active",
  });

  assert.equal(server.revokeDevice("device-1"), true);
  const revoked = await readStatus();
  assert.equal(revoked.status, 200);
  assert.equal((await revoked.json()).device_status, "revoked");
});
