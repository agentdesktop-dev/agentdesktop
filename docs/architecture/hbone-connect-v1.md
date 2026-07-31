# HBONE CONNECT contract v1

## Scope

This contract carries one captured TCP flow from the edge connector to Agent Gateway. It does not define traffic selection, TLS inspection policy, device enrollment, or application trust installation.

## Stream contract

- The outer transport is HTTP/2.
- Each captured TCP flow uses one `CONNECT` stream.
- The request authority is the original destination in `host:port` form. An explicit port is required.
- A `200 OK` response opens the tunnel. Any other status rejects it and must fail the captured flow closed.
- HTTP/2 DATA payloads are the original bidirectional TCP byte stream. The connector does not parse or transform inner TLS.
- End-of-stream in either direction represents that direction's TCP half-close.
- One HTTP/2 connection may multiplex multiple CONNECT streams only when they share the same deployment mode and authenticated identity context.
- The connector releases HTTP/2 receive flow-control capacity as bytes are consumed and applies sender flow control rather than buffering an unbounded stream.

## Authentication

Standalone local mode and managed remote mode use the same stream contract but different outer authentication.

Standalone local mode uses a local-only Agent Gateway endpoint. A private Unix socket is preferred if Agent Gateway gains compatible HTTP/2 listener support. The current prototype uses loopback and requires an opaque token from a current-user-owned `0600` file. The connector marks the token sensitive and sends it only as `x-agentgateway-edge-token` on CONNECT. Agent Gateway compares it through `source.connectHeaders` on the re-entered route, so it never enters the inner TCP stream. No organizational OAuth or device enrollment is required. Token creation, delivery to Agent Gateway, and rotation are not yet integrated into lifecycle management.

Managed remote mode requires TLS plus a short-lived DPoP-bound access token on every CONNECT request. Agent Gateway must validate the token, DPoP proof, replay uniqueness, method and target binding, then derive immutable user and device policy context. Connector authentication headers must not enter the inner TCP stream or provider request.

## Failure behavior

The connector never retries a CONNECT, replays inner bytes, or opens the original provider connection when connection setup, authentication, CONNECT response, or stream forwarding fails. After the HTTP/2 driver observes transport loss, a later flow may establish a new pooled connection. The failed or ambiguous flow remains failed closed. Transparent-capture rules must continue denying direct TCP and UDP/443 bypass while capture is enabled.

## Current implementation status

The connector implements and deterministically tests the HTTP/2 CONNECT stream primitive, explicit destination-port validation, bidirectional byte fidelity, flow-control release, half-close signaling, generation-safe lazy reconnect for later flows after observed transport loss, Linux original-destination recovery, bounded relay concurrency, and protected local-token loading. Private-container coverage validates cgroup v2/nftables redirection and UDP denial through the real relay. An opt-in smoke test proves local token rejection/acceptance and dynamic forwarding against a real Agent Gateway.

The following remain required before captured mode can be enabled:

- Managed TLS and per-CONNECT DPoP authentication.
- Connection pooling keyed by identity generation.
- Integrated standalone token creation, delivery, and rotation.
- Real Agent Gateway restart, cancellation, and stale-rule tests.
