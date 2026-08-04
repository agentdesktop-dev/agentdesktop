import { createHash, createPublicKey, verify } from "node:crypto";
import { readFile } from "node:fs/promises";
import http from "node:http";
import https from "node:https";
import { fileURLToPath } from "node:url";

function base64url(value) {
  return Buffer.from(value).toString("base64url");
}

function decodeJson(value) {
  return JSON.parse(Buffer.from(value, "base64url").toString("utf8"));
}

function jwkThumbprint(jwk) {
  return base64url(
    createHash("sha256")
      .update(JSON.stringify({ crv: jwk.crv, kty: jwk.kty, x: jwk.x, y: jwk.y }))
      .digest(),
  );
}

function verifyJwtSignature(jwt, publicKey) {
  const parts = jwt?.split(".");
  if (parts?.length !== 3) {
    throw new Error("invalid compact JWT");
  }
  const valid = verify(
    "sha256",
    Buffer.from(`${parts[0]}.${parts[1]}`),
    { key: publicKey, dsaEncoding: "ieee-p1363" },
    Buffer.from(parts[2], "base64url"),
  );
  if (!valid) {
    throw new Error("invalid JWT signature");
  }
  return { header: decodeJson(parts[0]), claims: decodeJson(parts[1]) };
}

function identityError(response, code, message) {
  response.writeHead(407, {
    "content-type": "application/json",
    "proxy-authenticate": "DPoP",
  });
  response.end(JSON.stringify({ error: { code, message } }));
}

function rejectIdentity(request, response, code, message) {
  if (request.readableEnded) {
    identityError(response, code, message);
    return;
  }
  request.resume();
  request.on("end", () => identityError(response, code, message));
}

async function loadIssuerConfiguration(issuer, audience, requiredScope) {
  const metadataUrl = new URL(".well-known/oauth-authorization-server", issuer);
  const metadata = await fetch(metadataUrl).then((response) => {
    if (!response.ok) {
      throw new Error(`issuer discovery failed with status ${response.status}`);
    }
    return response.json();
  });
  if (metadata.issuer !== issuer) {
    throw new Error("issuer discovery returned a mismatched issuer");
  }
  const jwks = await fetch(metadata.jwks_uri).then((response) => {
    if (!response.ok) {
      throw new Error(`issuer JWKS failed with status ${response.status}`);
    }
    return response.json();
  });
  const keys = new Map();
  for (const jwk of jwks.keys ?? []) {
    if (jwk.kid && jwk.kty === "EC" && jwk.crv === "P-256" && jwk.alg === "ES256") {
      keys.set(jwk.kid, createPublicKey({ key: jwk, format: "jwk" }));
    }
  }
  if (keys.size === 0) {
    throw new Error("issuer JWKS has no allowed ES256 key");
  }
  return { issuer, audience, keys, requiredScope };
}

function validateAccessToken(token, configuration, now) {
  const encodedHeader = token?.split(".")[0];
  if (!encodedHeader) {
    throw new Error("missing access token header");
  }
  const header = decodeJson(encodedHeader);
  if (header.typ !== "at+jwt" || header.alg !== "ES256" || !header.kid) {
    throw new Error("invalid access token header");
  }
  const key = configuration.keys.get(header.kid);
  if (!key) {
    throw new Error("untrusted access token key");
  }
  const { claims } = verifyJwtSignature(token, key);
  const audience = Array.isArray(claims.aud) ? claims.aud : [claims.aud];
  const scopes = typeof claims.scope === "string" ? claims.scope.split(" ") : [];
  if (
    claims.iss !== configuration.issuer ||
    !audience.includes(configuration.audience) ||
    !scopes.includes(configuration.requiredScope) ||
    typeof claims.sub !== "string" ||
    !claims.sub ||
    typeof claims.jti !== "string" ||
    !claims.jti ||
    typeof claims.iat !== "number" ||
    claims.iat > now + 60 ||
    typeof claims.exp !== "number" ||
    claims.exp <= now ||
    typeof claims.cnf?.jkt !== "string"
  ) {
    throw new Error("invalid access token claims");
  }
  return claims;
}

function validateDpopProof(proof, token, tokenClaims, method, targetUrl, now) {
  const parts = proof?.split(".");
  if (parts?.length !== 3) {
    throw new Error("invalid DPoP proof");
  }
  const header = decodeJson(parts[0]);
  if (
    header.typ !== "dpop+jwt" ||
    header.alg !== "ES256" ||
    header.jwk?.kty !== "EC" ||
    header.jwk?.crv !== "P-256"
  ) {
    throw new Error("invalid DPoP header");
  }
  const proofKey = createPublicKey({ key: header.jwk, format: "jwk" });
  const { claims } = verifyJwtSignature(proof, proofKey);
  if (
    claims.htm !== method ||
    claims.htu !== targetUrl ||
    typeof claims.iat !== "number" ||
    Math.abs(now - claims.iat) > 60 ||
    typeof claims.jti !== "string" ||
    !claims.jti ||
    claims.ath !== base64url(createHash("sha256").update(token).digest()) ||
    jwkThumbprint(header.jwk) !== tokenClaims.cnf.jkt
  ) {
    throw new Error("invalid DPoP claims");
  }
  return claims.jti;
}

export async function startFakeManagedGateway({
  issuer,
  audience,
  requiredScope,
  publicOrigin,
  provider,
  listenHost = "127.0.0.1",
  port = 0,
  tls,
} = {}) {
  if (!issuer || !audience || !requiredScope || !provider) {
    throw new Error("fake managed gateway requires issuer, audience, scope, and provider");
  }
  const configuration = await loadIssuerConfiguration(issuer, audience, requiredScope);
  const providerUrl = new URL(provider);
  let gatewayOrigin = publicOrigin ? new URL(publicOrigin) : undefined;
  const acceptedProofs = new Set();

  const handleRequest = (request, response) => {
    const authorization = request.headers["proxy-authorization"]?.match(/^DPoP (.+)$/);
    const token = authorization?.[1];
    if (!token || !request.headers.dpop) {
      return rejectIdentity(request, response, "identity_token_missing", "managed identity is required");
    }

    let tokenClaims;
    try {
      const now = Math.floor(Date.now() / 1000);
      tokenClaims = validateAccessToken(token, configuration, now);
    } catch {
      return rejectIdentity(request, response, "identity_token_invalid", "managed identity token is invalid");
    }

    let proofIdentifier;
    try {
      const now = Math.floor(Date.now() / 1000);
      const targetUrl = new URL(request.url, gatewayOrigin).toString();
      proofIdentifier = validateDpopProof(
        request.headers.dpop,
        token,
        tokenClaims,
        request.method,
        targetUrl,
        now,
      );
    } catch {
      return rejectIdentity(request, response, "dpop_proof_invalid", "managed identity proof is invalid");
    }
    if (acceptedProofs.has(proofIdentifier)) {
      return rejectIdentity(request, response, "dpop_proof_replayed", "managed identity proof was replayed");
    }
    acceptedProofs.add(proofIdentifier);

    const headers = { ...request.headers };
    delete headers.host;
    delete headers["proxy-authorization"];
    delete headers.dpop;
    const upstream = http.request(
      new URL(request.url, providerUrl),
      { method: request.method, headers },
      (upstreamResponse) => {
        response.writeHead(upstreamResponse.statusCode ?? 502, upstreamResponse.headers);
        upstreamResponse.pipe(response);
      },
    );
    upstream.on("error", () => {
      if (!response.headersSent) {
        response.writeHead(502, { "content-type": "application/json" });
        response.end(JSON.stringify({ error: { code: "provider_unavailable" } }));
      } else {
        response.destroy();
      }
    });
    request.pipe(upstream);
  };
  const server = tls ? https.createServer(tls, handleRequest) : http.createServer(handleRequest);
  await new Promise((resolve) => server.listen(port, listenHost, resolve));
  const boundPort = server.address().port;
  gatewayOrigin ??= new URL(`${tls ? "https" : "http"}://${listenHost}:${boundPort}`);
  return {
    close: () => new Promise((resolve, reject) => server.close((error) => error ? reject(error) : resolve())),
    origin: gatewayOrigin.toString(),
    port: boundPort,
  };
}

if (process.argv[1] && fileURLToPath(import.meta.url) === process.argv[1]) {
  const keyPath = process.env.AGENTDESKTOP_FAKE_TLS_KEY;
  const certificatePath = process.env.AGENTDESKTOP_FAKE_TLS_CERTIFICATE;
  if (!keyPath || !certificatePath) {
    throw new Error("fake managed gateway requires TLS key and certificate paths");
  }
  const server = await startFakeManagedGateway({
    issuer: process.env.AGENTDESKTOP_FAKE_ISSUER,
    audience: process.env.AGENTDESKTOP_FAKE_AUDIENCE,
    requiredScope: process.env.AGENTDESKTOP_FAKE_SCOPE,
    publicOrigin: process.env.AGENTDESKTOP_FAKE_GATEWAY_ORIGIN,
    provider: process.env.AGENTDESKTOP_FAKE_PROVIDER,
    listenHost: process.env.AGENTDESKTOP_FAKE_LISTEN_HOST ?? "127.0.0.1",
    port: Number(process.env.AGENTDESKTOP_FAKE_PORT ?? "4000"),
    tls: {
      key: await readFile(keyPath),
      cert: await readFile(certificatePath),
    },
  });
  console.log(`fake managed gateway listening on port ${server.port}`);
  const close = async () => {
    await server.close();
    process.exit(0);
  };
  process.on("SIGINT", close);
  process.on("SIGTERM", close);
}