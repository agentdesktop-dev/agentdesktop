import {
  createHash,
  createPublicKey,
  generateKeyPairSync,
  randomUUID,
  sign,
  verify,
} from "node:crypto";
import { readFile } from "node:fs/promises";
import { createServer as createHttpServer } from "node:http";
import { createServer as createHttpsServer } from "node:https";
import { fileURLToPath } from "node:url";

const clientId = "agentgateway-edge-test";
const audience = "agentgateway-edge";
const scope = "agentgateway.invoke";
const subject = "test-user";

function base64url(value) {
  return Buffer.from(value).toString("base64url");
}

function encodeJson(value) {
  return base64url(JSON.stringify(value));
}

function decodeJson(value) {
  return JSON.parse(Buffer.from(value, "base64url").toString("utf8"));
}

function sha256(value) {
  return createHash("sha256").update(value).digest();
}

function jwkThumbprint(jwk) {
  return base64url(
    sha256(
      JSON.stringify({ crv: jwk.crv, kty: jwk.kty, x: jwk.x, y: jwk.y }),
    ),
  );
}

function signJwt(header, claims, privateKey) {
  const input = `${encodeJson(header)}.${encodeJson(claims)}`;
  const signature = sign("sha256", Buffer.from(input), {
    key: privateKey,
    dsaEncoding: "ieee-p1363",
  });
  return `${input}.${base64url(signature)}`;
}

function verifyDpop(proof, method, targetUrl, accessToken) {
  const parts = proof?.split(".");
  if (parts?.length !== 3) {
    throw new Error("invalid DPoP proof");
  }
  const [encodedHeader, encodedClaims, encodedSignature] = parts;
  const header = decodeJson(encodedHeader);
  const claims = decodeJson(encodedClaims);
  if (header.typ !== "dpop+jwt" || header.alg !== "ES256" || !header.jwk) {
    throw new Error("invalid DPoP header");
  }
  if (
    claims.htm !== method ||
    claims.htu !== targetUrl ||
    typeof claims.jti !== "string" ||
    (accessToken && claims.ath !== base64url(sha256(accessToken))) ||
    Math.abs(Math.floor(Date.now() / 1000) - claims.iat) > 60
  ) {
    throw new Error("invalid DPoP claims");
  }
  const publicKey = createPublicKey({ key: header.jwk, format: "jwk" });
  const valid = verify(
    "sha256",
    Buffer.from(`${encodedHeader}.${encodedClaims}`),
    { key: publicKey, dsaEncoding: "ieee-p1363" },
    Buffer.from(encodedSignature, "base64url"),
  );
  if (!valid) {
    throw new Error("invalid DPoP signature");
  }
  return { jwk: header.jwk, claims };
}

function json(response, status, body) {
  response.writeHead(status, { "content-type": "application/json" });
  response.end(JSON.stringify(body));
}

async function readForm(request) {
  const chunks = [];
  for await (const chunk of request) {
    chunks.push(chunk);
  }
  return new URLSearchParams(Buffer.concat(chunks).toString("utf8"));
}

export async function startFakeAuthorizationServer({
  issuer: configuredIssuer,
  listenHost = "127.0.0.1",
  port = 0,
  tls,
  autoApprove = false,
} = {}) {
  const codes = new Map();
  const refreshTokens = new Set();
  const accessTokens = new Map();
  const enrollmentProofs = new Set();
  const enrollments = new Map();
  const devices = new Map();
  const signingKeys = generateKeyPairSync("ec", { namedCurve: "P-256" });
  const publicJwk = signingKeys.publicKey.export({ format: "jwk" });
  publicJwk.use = "sig";
  publicJwk.alg = "ES256";
  publicJwk.kid = "fake-signing-key";

  let issuer = configuredIssuer;
  const handleRequest = async (request, response) => {
    const url = new URL(request.url, issuer);
    if (request.method === "GET" && url.pathname === "/.well-known/oauth-authorization-server") {
      return json(response, 200, {
        issuer,
        authorization_endpoint: `${issuer}authorize`,
        token_endpoint: `${issuer}token`,
        enrollment_endpoint: `${issuer}enrollments`,
        jwks_uri: `${issuer}jwks`,
        response_types_supported: ["code"],
        code_challenge_methods_supported: ["S256"],
        dpop_signing_alg_values_supported: ["ES256"],
      });
    }
    if (request.method === "GET" && url.pathname === "/jwks") {
      return json(response, 200, { keys: [publicJwk] });
    }
    if (request.method === "GET" && url.pathname === "/authorize") {
      if (
        url.searchParams.get("response_type") !== "code" ||
        url.searchParams.get("client_id") !== clientId ||
        url.searchParams.get("scope") !== scope ||
        url.searchParams.get("code_challenge_method") !== "S256"
      ) {
        return json(response, 400, { error: "invalid_request" });
      }
      const redirectUri = url.searchParams.get("redirect_uri");
      const state = url.searchParams.get("state");
      const challenge = url.searchParams.get("code_challenge");
      if (!redirectUri || !state || !challenge) {
        return json(response, 400, { error: "invalid_request" });
      }
      const code = randomUUID();
      codes.set(code, { redirectUri, challenge });
      const redirect = new URL(redirectUri);
      redirect.searchParams.set("code", code);
      redirect.searchParams.set("state", state);
      response.writeHead(302, { location: redirect.toString() });
      return response.end();
    }
    if (request.method === "POST" && url.pathname === "/token") {
      try {
        const form = await readForm(request);
        if (form.get("client_id") !== clientId) {
          return json(response, 400, { error: "invalid_grant" });
        }
        const proofJwk = verifyDpop(request.headers.dpop, "POST", `${issuer}token`).jwk;
        if (form.get("grant_type") === "authorization_code") {
          const code = form.get("code");
          const authorization = codes.get(code);
          if (
            !authorization ||
            form.get("redirect_uri") !== authorization.redirectUri ||
            base64url(sha256(form.get("code_verifier") ?? "")) !== authorization.challenge
          ) {
            return json(response, 400, { error: "invalid_grant" });
          }
          codes.delete(code);
        } else if (form.get("grant_type") === "refresh_token") {
          const refreshToken = form.get("refresh_token");
          if (!refreshTokens.delete(refreshToken)) {
            return json(response, 400, { error: "invalid_grant" });
          }
        } else {
          return json(response, 400, { error: "unsupported_grant_type" });
        }
        const now = Math.floor(Date.now() / 1000);
        const refreshToken = randomUUID();
        refreshTokens.add(refreshToken);
        const accessToken = signJwt(
          { typ: "at+jwt", alg: "ES256", kid: publicJwk.kid },
          {
            iss: issuer,
            aud: audience,
            sub: subject,
            iat: now,
            exp: now + 300,
            jti: randomUUID(),
            scope,
            cnf: { jkt: jwkThumbprint(proofJwk) },
          },
          signingKeys.privateKey,
        );
        accessTokens.set(accessToken, {
          sub: subject,
          jkt: jwkThumbprint(proofJwk),
        });
        return json(response, 200, {
          access_token: accessToken,
          token_type: "DPoP",
          expires_in: 300,
          scope,
          refresh_token: refreshToken,
        });
      } catch {
        return json(response, 400, { error: "invalid_dpop_proof" });
      }
    }
    if (
      (request.method === "POST" && url.pathname === "/enrollments") ||
      (request.method === "GET" && url.pathname.startsWith("/enrollments/"))
    ) {
      try {
        const authorization = request.headers.authorization?.match(/^DPoP (.+)$/);
        const accessToken = authorization?.[1];
        const identity = accessTokens.get(accessToken);
        if (!accessToken || !identity) {
          return json(response, 401, { error: "invalid_token" });
        }
        const targetUrl = new URL(url.pathname, issuer).toString();
        const proof = verifyDpop(
          request.headers.dpop,
          request.method,
          targetUrl,
          accessToken,
        );
        if (jwkThumbprint(proof.jwk) !== identity.jkt) {
          return json(response, 401, { error: "invalid_dpop_proof" });
        }
        if (enrollmentProofs.has(proof.claims.jti)) {
          return json(response, 401, { error: "dpop_proof_replayed" });
        }
        enrollmentProofs.add(proof.claims.jti);

        if (request.method === "POST") {
          const enrollmentId = randomUUID();
          const enrollment = {
            enrollment_id: enrollmentId,
            status: "pending",
            user: { iss: issuer, sub: identity.sub },
            dpop_jkt: identity.jkt,
          };
          enrollments.set(enrollmentId, enrollment);
          if (autoApprove) {
            setTimeout(() => {
              const pending = enrollments.get(enrollmentId);
              if (pending?.status === "pending") {
                const deviceId = randomUUID();
                pending.status = "approved";
                pending.device_id = deviceId;
                devices.set(deviceId, "active");
              }
            }, 500);
          }
          return json(response, 202, enrollment);
        }

        const enrollmentId = url.pathname.slice("/enrollments/".length);
        const enrollment = enrollments.get(enrollmentId);
        if (
          !enrollment ||
          enrollment.user.sub !== identity.sub ||
          enrollment.dpop_jkt !== identity.jkt
        ) {
          return json(response, 404, { error: "enrollment_not_found" });
        }
        const deviceStatus = enrollment.device_id
          ? devices.get(enrollment.device_id)
          : undefined;
        return json(response, 200, {
          ...enrollment,
          ...(deviceStatus ? { device_status: deviceStatus } : {}),
        });
      } catch {
        return json(response, 401, { error: "invalid_dpop_proof" });
      }
    }
    return json(response, 404, { error: "not_found" });
  };
  const server = tls
    ? createHttpsServer(tls, handleRequest)
    : createHttpServer(handleRequest);

  await new Promise((resolve) => server.listen(port, listenHost, resolve));
  const address = server.address();
  issuer ??= `${tls ? "https" : "http"}://127.0.0.1:${address.port}/`;

  return {
    issuer,
    clientId,
    audience,
    scope,
    approveEnrollment(enrollmentId, deviceId = randomUUID()) {
      const enrollment = enrollments.get(enrollmentId);
      if (!enrollment || enrollment.status !== "pending") {
        return false;
      }
      enrollment.status = "approved";
      enrollment.device_id = deviceId;
      devices.set(deviceId, "active");
      return true;
    },
    revokeDevice(deviceId) {
      if (!devices.has(deviceId)) {
        return false;
      }
      devices.set(deviceId, "revoked");
      return true;
    },
    close: () => new Promise((resolve, reject) => server.close((error) => error ? reject(error) : resolve())),
  };
}

export const fakeAuthorizationInternals = {
  base64url,
  decodeJson,
  jwkThumbprint,
  signJwt,
};

if (process.argv[1] && fileURLToPath(import.meta.url) === process.argv[1]) {
  const tlsKey = process.env.AGENTGATEWAY_EDGE_FAKE_TLS_KEY;
  const tlsCertificate = process.env.AGENTGATEWAY_EDGE_FAKE_TLS_CERTIFICATE;
  if (Boolean(tlsKey) !== Boolean(tlsCertificate)) {
    throw new Error("both fake TLS key and certificate paths are required");
  }
  const tls = tlsKey
    ? {
        key: await readFile(tlsKey),
        cert: await readFile(tlsCertificate),
      }
    : undefined;
  const server = await startFakeAuthorizationServer({
    issuer: process.env.AGENTGATEWAY_EDGE_FAKE_ISSUER,
    listenHost: process.env.AGENTGATEWAY_EDGE_FAKE_LISTEN_HOST ?? "127.0.0.1",
    port: Number(process.env.AGENTGATEWAY_EDGE_FAKE_PORT ?? "0"),
    tls,
    autoApprove: process.env.AGENTGATEWAY_EDGE_FAKE_AUTO_APPROVE === "1",
  });
  console.log(server.issuer);
  const close = async () => {
    await server.close();
    process.exit(0);
  };
  process.on("SIGINT", close);
  process.on("SIGTERM", close);
}
