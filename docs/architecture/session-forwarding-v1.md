# Session forwarding contract v1

Status: implementation in progress.

Agent Desktop uses one machine-owned forwarder and one control agent in each active user session. The machine forwarder owns native listeners, transparent capture, original-destination recovery, connection attribution, HBONE pools, and fail-closed behavior. It never persists OAuth tokens or private-key bytes. A user agent owns OAuth login and refresh, enrollment and certificate renewal, and access to the user's credential store.

Native applications use one machine loopback listener. The forwarder derives the connecting operating-system user without parsing application bytes. Linux uses an exact client/server tuple query through `NETLINK_SOCK_DIAG`; Windows uses WFP connection metadata; macOS uses Network Extension flow metadata. Missing, stale, or ambiguous attribution closes the connection. Transparent capture uses the same platform-derived identity and never accepts a user identifier from application traffic.

## Local session channel

The user agent connects to a machine-owned local IPC endpoint. The forwarder derives the peer UID, SID, or audit identity from the operating system before reading registration data. Registration contains only the protocol version, certificate generation, and DER certificate chain. Frames are length-prefixed JSON, reject unknown fields, and are limited to 1 MiB.

The user agent retains the mTLS private key. When rustls opens a new pooled Gateway connection, the forwarder sends the TLS signing input and selected signature scheme over the authenticated session channel. The user agent signs through its credential backend and returns only the signature. Signing is timeout-bounded because rustls exposes a synchronous signer interface. Existing pooled connections do not require IPC per captured flow.

One HBONE pool exists per operating-system user, Gateway authority, and certificate generation. Session disconnect, logout, certificate replacement, attribution failure, signing failure, or certificate expiry drains the affected pool and rejects new traffic. No pool or signing channel may be shared between users.

## Self-managed mode

The user agent owns its local Agent Gateway process and configuration. It registers the resulting local endpoint through authenticated IPC; the machine forwarder validates the registration against the IPC peer and forwards that user's native and captured traffic to that endpoint. Agent Gateway remains the only policy and provider-credential owner. The machine forwarder neither reads nor modifies user policy.