# Session forwarding contract v1

Status: Linux implementation complete. Windows named-pipe SID authentication, external signing, SID-keyed registration, and native WFP flow attribution are implemented. macOS transport remains pending.

Agent Desktop uses one machine-owned forwarder and one control agent in each active user session. The machine forwarder owns native listeners, transparent capture, original-destination recovery, connection attribution, HBONE pools, and fail-closed behavior. It never persists OAuth tokens or private-key bytes. A user agent owns OAuth login and refresh, enrollment and certificate renewal, and access to the user's credential store.

Native applications use one machine loopback listener. The forwarder derives the connecting operating-system user without parsing application bytes. Linux uses an exact client/server tuple query through `NETLINK_SOCK_DIAG`; Windows uses WFP connection metadata; macOS uses Network Extension flow metadata. Missing, stale, or ambiguous attribution closes the connection. Transparent capture uses the same platform-derived identity and never accepts a user identifier from application traffic.

On Windows, a machine-only WFP callout redirects the configured loopback destination to a hidden service listener. The callout obtains `TokenUser` from WFP's flow-bound authorization-token metadata, preserves the original sockaddr, and attaches both values as a versioned redirect context. It never attributes by PID or TCP-table snapshot. The machine service accepts only native contexts whose original destination exactly matches the configured public listener and whose SID has a live registration.

## Local session channel

The user agent connects to a machine-owned local IPC endpoint. The forwarder derives the peer UID, SID, or audit identity from the operating system before reading registration data. A registration contains exactly one identity form: a certificate generation and DER certificate chain for managed mode, or a loopback Gateway endpoint and bounded connector capability for self-managed mode. Frames are length-prefixed JSON, reject unknown fields, and are limited to 1 MiB.

The user agent retains the mTLS private key. When rustls opens a new pooled Gateway connection, the forwarder sends the TLS signing input and selected signature scheme over the authenticated session channel. The user agent signs through its credential backend and returns only the signature. Signing is timeout-bounded because rustls exposes a synchronous signer interface. Existing pooled connections do not require IPC per captured flow.

One HBONE pool exists per operating-system user, Gateway authority, and certificate generation. Session disconnect, logout, certificate replacement, attribution failure, signing failure, or certificate expiry drains the affected pool and rejects new traffic. No pool or signing channel may be shared between users.

## Self-managed mode

The user agent owns and supervises its local Agent Gateway process and configuration. It registers the resulting loopback endpoint and generated connector capability through authenticated IPC. The machine forwarder validates the registration, uses the capability only as a sensitive CONNECT header to that endpoint, and forwards the peer user's native and captured traffic there. The open IPC connection is the registration lease; disconnect evicts the matching generation. Agent Gateway remains the only policy and provider-credential owner. The machine forwarder neither reads nor modifies user policy.