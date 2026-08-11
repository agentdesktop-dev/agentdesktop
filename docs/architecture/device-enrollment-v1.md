# Device enrollment API v1 draft

Status: superseded DPoP-based fixture contract. Production enrollment follows [Managed mTLS identity contract v1](managed-mtls-v1.md). This document describes only the retained JavaScript authorization-server and gateway fixtures; the Rust connector no longer implements this enrollment protocol.

The production code reading order and enrollment sequence are maintained in [CONTRIBUTING.md](../../CONTRIBUTING.md#enrollment). Keep this draft aligned only with `tests/fixtures/fake-authorization-server.mjs` and its tests.

## Scope

This draft defines the executable mock authority used to develop managed device enrollment. It upgrades a DPoP thumbprint from connector-instance proof to an authority-approved device association. It does not make a local device name, connector-supplied ID, or unapproved key into verified device identity.

The mock implementation lives in `tests/fixtures/fake-authorization-server.mjs`. Production authority ownership, administrator authentication, durable storage, audit, and availability remain deployment decisions.

## Discovery

OAuth authorization-server metadata advertises:

```json
{"enrollment_endpoint":"https://issuer.example/enrollments"}
```

Enrollment calls use the current DPoP-bound access token:

```http
Authorization: DPoP <access-token>
DPoP: <fresh-proof-with-ath>
```

The authority validates the token, proof signature, `htm`, exact `htu`, freshness, unique `jti`, `ath`, and equality between the proof JWK thumbprint and `cnf.jkt`. It derives the canonical user from validated `(iss, sub)`. Request JSON cannot supply or override the user, thumbprint, approval state, or device ID.

## Request

`POST /enrollments` returns `202 Accepted`:

```json
{
  "enrollment_id": "authority-generated-id",
  "status": "pending",
  "user": {"iss": "https://issuer.example/", "sub": "stable-user-id"},
  "dpop_jkt": "validated-thumbprint"
}
```

Approval is an authority-side administrative action. The mock exposes it only as a test control and assigns the device ID there. A client cannot approve itself.

## Status and revocation

`GET /enrollments/{id}` requires a fresh proof from the same token-bound key. Another user or key receives `404` so enrollment existence is not disclosed. An approved response adds authority-derived fields:

```json
{
  "enrollment_id": "authority-generated-id",
  "status": "approved",
  "user": {"iss": "https://issuer.example/", "sub": "stable-user-id"},
  "dpop_jkt": "validated-thumbprint",
  "device_id": "authority-assigned-device-id",
  "device_status": "active"
}
```

Revocation changes `device_status` to `revoked` without revoking the user or changing the key association. Agent Gateway must consume an authenticated, fail-closed source of this status before exposing verified device identity or returning `device_revoked`.

## Fixture boundary

Fixture tests prove token/key binding, wrong-key rejection, proof replay rejection, explicit approval, authority-assigned device identity, and independent revocation state. They remain regression coverage for the experimental DPoP forwarding path and must not be treated as the production enrollment contract.