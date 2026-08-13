import http from "node:http";
import https from "node:https";
import fs from "node:fs";

const port = Number(process.env.MOCK_ANTHROPIC_PORT ?? "8000");
const host = process.env.MOCK_ANTHROPIC_HOST ?? "0.0.0.0";
const model = "claude-sonnet-4-20250514";

const message = {
  id: "msg_smoke",
  type: "message",
  role: "assistant",
  content: [{ type: "text", text: "SMOKE_OK" }],
  model,
  stop_reason: "end_turn",
  stop_sequence: null,
  usage: { input_tokens: 1, output_tokens: 1 },
};

const streamEvents = [
  [
    "message_start",
    {
      type: "message_start",
      message: {
        ...message,
        content: [],
        stop_reason: null,
        usage: { input_tokens: 1, output_tokens: 0 },
      },
    },
  ],
  [
    "content_block_start",
    {
      type: "content_block_start",
      index: 0,
      content_block: { type: "text", text: "" },
    },
  ],
  [
    "content_block_delta",
    {
      type: "content_block_delta",
      index: 0,
      delta: { type: "text_delta", text: "SMOKE_OK" },
    },
  ],
  ["content_block_stop", { type: "content_block_stop", index: 0 }],
  [
    "message_delta",
    {
      type: "message_delta",
      delta: { stop_reason: "end_turn", stop_sequence: null },
      usage: { output_tokens: 1 },
    },
  ],
  ["message_stop", { type: "message_stop" }],
];

function respondJson(response, status, body) {
  response.writeHead(status, { "content-type": "application/json" });
  response.end(JSON.stringify(body));
}

function respondStream(response) {
  response.writeHead(200, {
    "content-type": "text/event-stream",
    "cache-control": "no-cache",
  });
  response.end(
    `${streamEvents
      .map(([event, data]) => `event: ${event}\ndata: ${JSON.stringify(data)}`)
      .join("\n\n")}\n\n`,
  );
}

const handleRequest = (request, response) => {
  if (request.method !== "POST") {
    respondJson(response, 405, { type: "error", error: { type: "invalid_request_error", message: "method not allowed" } });
    return;
  }

  let body = "";
  request.setEncoding("utf8");
  request.on("data", (chunk) => {
    body += chunk;
  });
  request.on("end", () => {
    const pathname = new URL(request.url, "http://localhost").pathname;
    if (pathname === "/v1/messages/count_tokens") {
      respondJson(response, 200, { input_tokens: 1 });
      return;
    }
    if (pathname !== "/v1/messages") {
      respondJson(response, 404, { type: "error", error: { type: "not_found_error", message: "not found" } });
      return;
    }

    let payload;
    try {
      payload = JSON.parse(body);
    } catch {
      respondJson(response, 400, { type: "error", error: { type: "invalid_request_error", message: "invalid JSON" } });
      return;
    }

    if (payload.stream) {
      respondStream(response);
    } else {
      respondJson(response, 200, message);
    }
  });
};

const tlsCertificate = process.env.MOCK_ANTHROPIC_TLS_CERTIFICATE;
const tlsKey = process.env.MOCK_ANTHROPIC_TLS_KEY;
if (Boolean(tlsCertificate) !== Boolean(tlsKey)) {
  throw new Error("mock Anthropic TLS certificate and key must be provided together");
}
const server = tlsCertificate
  ? https.createServer(
      { cert: fs.readFileSync(tlsCertificate), key: fs.readFileSync(tlsKey) },
      handleRequest,
    )
  : http.createServer(handleRequest);

server.listen(port, host, () => {
  console.log(`mock Anthropic API listening on ${host}:${port}`);
});
