import {
  createHash,
  createPublicKey,
  generateKeyPairSync,
  randomUUID,
  sign,
  verify,
} from "node:crypto";
import { createServer } from "node:http";
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

function verifyDpop(proof, method, targetUrl) {
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
  return header.jwk;
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

export async function startFakeAuthorizationServer() {
  const codes = new Map();
  const signingKeys = generateKeyPairSync("ec", { namedCurve: "P-256" });
  const publicJwk = signingKeys.publicKey.export({ format: "jwk" });
  publicJwk.use = "sig";
  publicJwk.alg = "ES256";
  publicJwk.kid = "fake-signing-key";

  let issuer;
  const server = createServer(async (request, response) => {
    const url = new URL(request.url, issuer);
    if (request.method === "GET" && url.pathname === "/.well-known/oauth-authorization-server") {
      return json(response, 200, {
        issuer,
        authorization_endpoint: `${issuer}authorize`,
        token_endpoint: `${issuer}token`,
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
        const code = form.get("code");
        const authorization = codes.get(code);
        if (
          form.get("grant_type") !== "authorization_code" ||
          form.get("client_id") !== clientId ||
          !authorization ||
          form.get("redirect_uri") !== authorization.redirectUri ||
          base64url(sha256(form.get("code_verifier") ?? "")) !== authorization.challenge
        ) {
          return json(response, 400, { error: "invalid_grant" });
        }
        const proofJwk = verifyDpop(request.headers.dpop, "POST", `${issuer}token`);
        codes.delete(code);
        const now = Math.floor(Date.now() / 1000);
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
        return json(response, 200, {
          access_token: accessToken,
          token_type: "DPoP",
          expires_in: 300,
          scope,
        });
      } catch {
        return json(response, 400, { error: "invalid_dpop_proof" });
      }
    }
    return json(response, 404, { error: "not_found" });
  });

  await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
  const address = server.address();
  issuer = `http://127.0.0.1:${address.port}/`;

  return {
    issuer,
    clientId,
    audience,
    scope,
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
  const server = await startFakeAuthorizationServer();
  console.log(server.issuer);
  const close = async () => {
    await server.close();
    process.exit(0);
  };
  process.on("SIGINT", close);
  process.on("SIGTERM", close);
}
