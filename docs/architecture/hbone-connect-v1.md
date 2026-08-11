# HBONE CONNECT contract v1

## Scope

This contract carries one native or captured TCP flow from the edge connector to Agent Gateway. It does not define traffic selection, TLS inspection policy, device enrollment, or application trust installation.

## Stream contract

- The outer transport is HTTP/2.
- Each application TCP flow uses one `CONNECT` stream.
- Native forwarding uses the configured fixed internal authority. Captured forwarding uses the original destination in `host:port` form. An explicit port is required.
- A `200 OK` response opens the tunnel. Any other status rejects it and must fail the captured flow closed.
- HTTP/2 DATA payloads are the original bidirectional TCP byte stream. The connector does not parse or transform inner TLS.
- End-of-stream in either direction represents that direction's TCP half-close.
- One HTTP/2 connection may multiplex multiple CONNECT streams only when they share the same deployment mode and authenticated identity context.
- The connector releases HTTP/2 receive flow-control capacity as bytes are consumed and applies sender flow control rather than buffering an unbounded stream.

## Authentication

Standalone local mode and managed remote mode use the same stream contract but different outer authentication.

Standalone local mode uses a loopback Agent Gateway endpoint and one 256-bit capability per connector-owned Gateway process. Agent Desktop generates the capability in memory, injects it into the Gateway startup environment, and sends it only as `x-agentdesktop-token` on CONNECT. Agent Gateway compares it through `source.connectHeaders` on the re-entered route, so it never enters the inner TCP stream. The diagnostic `capture` subcommand can instead read the capability from a current-user-owned `0600` file. No organizational OAuth or device enrollment is required.

Managed remote mode requires mTLS with a short-lived enrollment certificate whose authority-issued SPIFFE URI binds organization, user, and device. Agent Gateway derives immutable identity from the validated certificate and keeps the connection isolated to that certificate generation. Managed CONNECT requests carry no OAuth credential or connector authentication header.

## Failure behavior

The connector never retries a CONNECT, replays inner bytes, or opens the original provider connection when connection setup, authentication, CONNECT response, or stream forwarding fails. After the HTTP/2 driver observes transport loss, a later flow may establish a new pooled connection. The failed or ambiguous flow remains failed closed. Transparent-capture rules must continue denying direct TCP and UDP/443 bypass while capture is enabled.

## Current implementation status

The connector implements and deterministically tests pooled plain and mTLS HTTP/2 connections, CONNECT streams, explicit destination-port validation, bidirectional byte fidelity, flow-control release, half-close signaling, generation-safe lazy reconnect for later flows after observed transport loss, Linux original-destination recovery, bounded relay concurrency, and standalone token lifecycle. Private-container coverage validates cgroup v2/nftables redirection and UDP denial through the real relay. Real Gateway smoke paths prove local token rejection/acceptance, native Claude forwarding, policy allow/deny, and dynamic captured forwarding.

The following remain required before managed captured mode can be enabled and before capture is production-ready across platforms:

- Immutable certificate-derived outer-to-inner identity propagation for managed capture.
- Published certificate revocation consumption and fail-closed rejection before certificate expiry.
- Production validation of Gateway restart, cancellation, existing-firewall interaction, and stale-rule recovery.
