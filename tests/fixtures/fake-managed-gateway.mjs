import { readFile } from "node:fs/promises";
import http from "node:http";
import https from "node:https";

const listenHost = process.env.AGENTGATEWAY_EDGE_FAKE_LISTEN_HOST ?? "127.0.0.1";
const listenPort = Number(process.env.AGENTGATEWAY_EDGE_FAKE_PORT ?? "4000");
const provider = new URL(
  process.env.AGENTGATEWAY_EDGE_FAKE_PROVIDER ?? "http://127.0.0.1:8000/",
);
const keyPath = process.env.AGENTGATEWAY_EDGE_FAKE_TLS_KEY;
const certificatePath = process.env.AGENTGATEWAY_EDGE_FAKE_TLS_CERTIFICATE;

if (!keyPath || !certificatePath) {
  throw new Error("fake managed gateway requires TLS key and certificate paths");
}

function respondJson(response, status, body) {
  response.writeHead(status, { "content-type": "application/json" });
  response.end(JSON.stringify(body));
}

const server = https.createServer(
  {
    key: await readFile(keyPath),
    cert: await readFile(certificatePath),
  },
  (request, response) => {
    if (
      !request.headers["proxy-authorization"]?.startsWith("DPoP ") ||
      !request.headers.dpop
    ) {
      respondJson(response, 401, {
        type: "error",
        error: { type: "authentication_error", message: "managed identity required" },
      });
      return;
    }

    const headers = { ...request.headers };
    delete headers.host;
    delete headers["proxy-authorization"];
    delete headers.dpop;
    const upstream = http.request(
      new URL(request.url, provider),
      {
        method: request.method,
        headers,
      },
      (upstreamResponse) => {
        response.writeHead(
          upstreamResponse.statusCode ?? 502,
          upstreamResponse.headers,
        );
        upstreamResponse.pipe(response);
      },
    );
    upstream.on("error", () => {
      if (!response.headersSent) {
        respondJson(response, 502, {
          type: "error",
          error: { type: "api_error", message: "mock provider unavailable" },
        });
      } else {
        response.destroy();
      }
    });
    request.pipe(upstream);
  },
);

server.listen(listenPort, listenHost, () => {
  console.log(`fake managed gateway listening on ${listenHost}:${listenPort}`);
});

const close = () => server.close(() => process.exit(0));
process.on("SIGINT", close);
process.on("SIGTERM", close);