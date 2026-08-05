# Managed mTLS identity contract v1

Status: selected production direction; enrollment request persistence, administrator-scoped listing, approval and rejection, retry-stable local protected-key issuance, interrupted issuance and renewal reconciliation, owner-scoped certificate retrieval and valid-certificate renewal, Agent Desktop key/certificate persistence, proactive renewal and credential rotation, connector-side mTLS activation, and atomic device/certificate revocation persistence are implemented. Production CA integration, expired-certificate recovery, revocation consumption, and Agent Gateway enforcement remain incomplete.

## Trust model

Managed mode uses ordinary OAuth access tokens for organizational user identity and a short-lived client certificate for device identity. The OAuth provider does not need DPoP support. Agent Desktop generates and retains the device private key; neither the enrollment service, PostgreSQL, nor Agent Gateway receives private-key bytes.

The enrollment service validates the OAuth token against configured issuer metadata and JWKS, derives the canonical user from validated `(iss, sub)`, validates a signed P-256 CSR, and records a pending enrollment. It must ignore client-controlled CSR subject and SAN values when issuing a certificate. Administrator approval assigns the device ID, and a CA adapter issues an authority-controlled certificate that binds the approved organization and device IDs to the submitted public key.

The enrollment service terminates TLS 1.3 directly. Initial enrollment may connect without a client certificate and relies on OAuth. Any presented client certificate must chain to the configured enrollment CA; renewal routes additionally require a verified certificate and derive device identity only from its authority-controlled SPIFFE URI. Reverse-proxy headers are never accepted as device identity.

PostgreSQL stores organizations, users, enrollment state, devices, certificate serials and validity, revocation state, and audit events. CA signing keys do not belong in PostgreSQL. Production issuance must use a separate CA implementation backed by protected signing keys such as `step-ca`, Vault PKI, a cloud private CA, KMS, or an HSM.

## Enrollment

Agent Desktop submits a PEM CSR with a standard OAuth bearer token:

```http
POST /v1/enrollments
Authorization: Bearer <access-token>
Content-Type: application/json

{"csr":"-----BEGIN CERTIFICATE REQUEST-----\n..."}
```

The service returns `202 Accepted` with a server-generated enrollment ID, pending status, canonical issuer and subject, and the validated public-key fingerprint. Request data cannot supply or override organization, user, approval, or device identity.

The organization bootstrap supplies a distinct HTTPS `enrollment_url`; Agent Desktop does not expect an external OAuth provider to advertise connector-specific enrollment metadata. Agent Desktop generates a P-256 key, stores its PKCS#8 private key through the selected protected credential backend, and verifies that the service fingerprint matches the CSR public key. It polls with ordinary bearer authentication and accepts an approved record only when the returned leaf certificate's SubjectPublicKeyInfo exactly matches the retained private key. The private key is never printed by enrollment commands.

Managed connector startup fails closed unless it can load an approved, key-matching certificate from protected storage. The reqwest/rustls upstream client presents that identity. OAuth and device credentials have independent generations on one upstream pool boundary, so rotating either rebuilds future Gateway connections without downgrading the other identity.

## Gateway authentication

Agent Gateway requires a client certificate chaining to the configured enrollment CA. It validates the chain, validity, client-auth usage, organization scope, and revocation status before constructing immutable device context. Agent Gateway separately validates the ordinary OAuth bearer token and constructs user context from its verified claims. The authenticated connection is isolated by organization, user, device, and certificate generation; inspected inner headers cannot override this context.

For HBONE, one mTLS HTTP/2 connection may carry multiple CONNECT streams only for that same immutable context. OAuth credentials are carried on the outer request and stripped before inner traffic, policy extensions, logs, traces, mirrors, or provider forwarding.

## Renewal and recovery

Before expiry, Agent Desktop generates a new local key and CSR and renews over mTLS using its current certificate. The enrollment service verifies that the device remains active, issues a replacement, and records the new certificate serial and validity. Agent Desktop atomically activates the new key and certificate, opens new Gateway connections, and drains old connections.

Agent Desktop checks the parsed leaf validity at startup and every 15 minutes, renewing within six hours of expiry. It persists the new private key as a protected draft before the request so retries after ambiguous failures use the same CSR fingerprint and server claim. A validated replacement is persisted and reloaded before its device generation is published to the proxy; failures retain the current in-memory identity and retry after one minute. The successful generation change rebuilds the reqwest pool for future connections while existing requests drain normally.

The service persists a renewal claim before CA I/O and keys it by device plus the new public-key fingerprint. The claim fixes the renewal ID, organization, device, CSR, and issuance time. Ambiguous CA failures remain `issuing`; a bounded worker retries them with the renewal ID as the external CA idempotency key. Completion locks the active device and claim, inserts the certificate, transitions the claim, and records audit in one transaction. Device revocation serializes on the same device row, so either renewal completes before revocation and the new certificate is revoked with the device, or renewal fails closed.

An expired certificate cannot authenticate ordinary mTLS renewal. Recovery uses a valid OAuth session plus proof of possession of the enrolled private key, such as a signature over a server nonce bound to the new CSR. The service may use the expired certificate only as identifying evidence. Recovery is allowed only within configured policy and for an approved, non-revoked device; otherwise full re-enrollment and administrator approval are required.

The current Go issuer adapter uses a protected local CA key and Go's X.509 implementation. It is a concrete development and single-instance boundary, not a final production key-management decision. The narrow issuer interface is intended to be replaced by KMS, HSM, `step-ca`, Vault PKI, or a cloud private CA without moving approval policy into the CA adapter.

Approval claims a pending enrollment as `issuing` before calling the CA, preventing concurrent duplicate approval without holding a database transaction across CA I/O. The claim fixes the enrollment ID, device ID, CSR, and issuance time. CA failures remain `issuing` because timeout errors cannot prove that no certificate was created. A bounded background worker retries stale claims with those same values, while transactional completion permits only one worker to persist and approve the result. External CA adapters must pass the enrollment ID through as an idempotency key.

## Remaining implementation

- Replace the local-key issuer with a production protected-key CA adapter.
- Add expired-certificate recovery.
- Add fail-closed Agent Gateway consumption of persisted device and certificate revocation state.
- Add Agent Gateway mTLS validation and immutable outer-to-inner identity propagation.