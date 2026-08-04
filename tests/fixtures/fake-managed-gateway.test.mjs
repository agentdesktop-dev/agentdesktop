import assert from "node:assert/strict";
import {
  createHash,
  generateKeyPairSync,
  randomBytes,
  randomUUID,
} from "node:crypto";
import http from "node:http";
import test from "node:test";

import {
  fakeAuthorizationInternals,
  startFakeAuthorizationServer,
} from "./fake-authorization-server.mjs";
import { startFakeManagedGateway } from "./fake-managed-gateway.mjs";

const { base64url, signJwt } = fakeAuthorizationInternals;

async function stage(name, operation) {
  try {
    return await operation;
  } catch (error) {
    throw new Error(`${name} failed`, { cause: error });
  }
}

function dpopProof(privateKey, publicKey, method, targetUrl, accessToken) {
  return signJwt(
    { typ: "dpop+jwt", alg: "ES256", jwk: publicKey.export({ format: "jwk" }) },
    {
      htm: method,
      htu: targetUrl,
      iat: Math.floor(Date.now() / 1000),
      jti: randomUUID(),
      ath: base64url(createHash("sha256").update(accessToken).digest()),
    },
    privateKey,
  );
}

async function issueToken(authority, keys) {
  const verifier = base64url(randomBytes(32));
  const redirectUri = "http://127.0.0.1:49152/callback";
  const authorize = new URL("authorize", authority.issuer);
  authorize.search = new URLSearchParams({
    response_type: "code",
    client_id: authority.clientId,
    redirect_uri: redirectUri,
    scope: authority.scope,
    state: "managed-gateway-test",
    code_challenge: base64url(createHash("sha256").update(verifier).digest()),
    code_challenge_method: "S256",
  });
  const authorization = await fetch(authorize, { redirect: "manual" });
  const code = new URL(authorization.headers.get("location")).searchParams.get("code");
  const tokenEndpoint = new URL("token", authority.issuer).toString();
  const token = await fetch(tokenEndpoint, {
    method: "POST",
    headers: {
      "content-type": "application/x-www-form-urlencoded",
      dpop: signJwt(
        { typ: "dpop+jwt", alg: "ES256", jwk: keys.publicKey.export({ format: "jwk" }) },
        {
          htm: "POST",
          htu: tokenEndpoint,
          iat: Math.floor(Date.now() / 1000),
          jti: randomUUID(),
        },
        keys.privateKey,
      ),
    },
    body: new URLSearchParams({
      grant_type: "authorization_code",
      client_id: authority.clientId,
      redirect_uri: redirectUri,
      code,
      code_verifier: verifier,
    }),
  });
  assert.equal(token.status, 200);
  return token.json();
}

async function startProvider() {
  const server = http.createServer((request, response) => {
    request.resume();
    request.on("end", () => {
      response.writeHead(200, { "content-type": "application/json" });
      response.end(JSON.stringify({ headers: request.headers }));
    });
  });
  await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
  return {
    close: () => new Promise((resolve, reject) => server.close((error) => error ? reject(error) : resolve())),
    origin: `http://127.0.0.1:${server.address().port}/`,
  };
}

function gatewayRequest(target, headers, body) {
  return new Promise((resolve, reject) => {
    const request = http.request(target, { method: "POST", headers }, (response) => {
      const chunks = [];
      response.on("data", (chunk) => chunks.push(chunk));
      response.on("end", () => resolve({
        body: JSON.parse(Buffer.concat(chunks).toString("utf8")),
        headers: response.headers,
        status: response.statusCode,
      }));
    });
    request.on("error", reject);
    request.end(body);
  });
}

test("validates DPoP identity, rejects replay, and strips credentials", async (context) => {
  const authority = await startFakeAuthorizationServer();
  const provider = await startProvider();
  const gateway = await stage("gateway startup", startFakeManagedGateway({
    issuer: authority.issuer,
    audience: authority.audience,
    requiredScope: authority.scope,
    provider: provider.origin,
  }));
  context.after(() => Promise.all([gateway.close(), provider.close(), authority.close()]));

  const keys = generateKeyPairSync("ec", { namedCurve: "P-256" });
  const token = await stage("token issuance", issueToken(authority, keys));
  const target = new URL("v1/messages", gateway.origin).toString();
  const proof = dpopProof(keys.privateKey, keys.publicKey, "POST", target, token.access_token);
  const headers = {
    "proxy-authorization": `DPoP ${token.access_token}`,
    dpop: proof,
  };
  const request = () => gatewayRequest(target, headers, "test");

  const accepted = await stage("accepted request", request());
  assert.equal(accepted.status, 200);
  const providerHeaders = accepted.body.headers;
  assert.equal(providerHeaders["proxy-authorization"], undefined);
  assert.equal(providerHeaders.dpop, undefined);

  const replayed = await stage("replayed request", gatewayRequest(target, headers));
  assert.equal(replayed.status, 407);
  assert.equal(replayed.body.error.code, "dpop_proof_replayed");

  const wrongKeys = generateKeyPairSync("ec", { namedCurve: "P-256" });
  const wrongProof = dpopProof(
    wrongKeys.privateKey,
    wrongKeys.publicKey,
    "POST",
    target,
    token.access_token,
  );
  const wrongKey = await stage("wrong-key request", gatewayRequest(target, {
    "proxy-authorization": `DPoP ${token.access_token}`,
    dpop: wrongProof,
  }));
  assert.equal(wrongKey.status, 407);
  assert.equal(wrongKey.body.error.code, "dpop_proof_invalid");

  const tokenParts = token.access_token.split(".");
  const tamperedToken = `${tokenParts[0]}.${base64url('{"iss":"attacker"}')}.${tokenParts[2]}`;
  const tamperedTokenResponse = await stage("tampered-token request", gatewayRequest(target, {
    "proxy-authorization": `DPoP ${tamperedToken}`,
    dpop: dpopProof(keys.privateKey, keys.publicKey, "POST", target, tamperedToken),
  }));
  assert.equal(tamperedTokenResponse.status, 407);
  assert.equal(tamperedTokenResponse.body.error.code, "identity_token_invalid");

  const missing = await stage("missing-identity request", gatewayRequest(target, {}));
  assert.equal(missing.status, 407);
  assert.equal(missing.headers["proxy-authenticate"], "DPoP");
  assert.equal(missing.body.error.code, "identity_token_missing");
});