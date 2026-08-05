package renewal

import (
	"context"
	"crypto/ecdsa"
	"crypto/elliptic"
	"crypto/rand"
	"crypto/sha256"
	"crypto/x509"
	"encoding/base64"
	"encoding/pem"
	"errors"
	"testing"
	"time"

	"github.com/agentdesktop-dev/agentdesktop/control-plane/internal/ca"
	"github.com/agentdesktop-dev/agentdesktop/control-plane/internal/certificate"
	"github.com/agentdesktop-dev/agentdesktop/control-plane/internal/deviceidentity"
	"github.com/agentdesktop-dev/agentdesktop/control-plane/internal/enrollment"
)

type recordingStore struct {
	claim             Claim
	recoveryChallenge RecoveryChallenge
	recoveryClaim     Claim
	completed         bool
	issuing           []Claim
}

func (store *recordingStore) CreateRecoveryChallenge(
	_ context.Context,
	_ enrollment.Principal,
	deviceID string,
	serial string,
	request certificate.Request,
	id string,
	nonce []byte,
	expiresAt time.Time,
) (RecoveryChallenge, error) {
	store.recoveryChallenge.ID = id
	store.recoveryChallenge.DeviceID = deviceID
	store.recoveryChallenge.PresentedSerialNumber = serial
	store.recoveryChallenge.CSRDER = request.DER
	store.recoveryChallenge.PublicKeyFingerprint = request.PublicKeyFingerprint
	store.recoveryChallenge.Nonce = nonce
	store.recoveryChallenge.ExpiresAt = expiresAt
	return store.recoveryChallenge, nil
}

func (store *recordingStore) GetRecoveryChallenge(
	context.Context,
	enrollment.Principal,
	string,
) (RecoveryChallenge, error) {
	return store.recoveryChallenge, nil
}

func (store *recordingStore) BeginRecovery(
	context.Context,
	enrollment.Principal,
	RecoveryChallenge,
	string,
) (Claim, error) {
	return store.recoveryClaim, nil
}

func (store *recordingStore) Begin(
	context.Context,
	enrollment.Principal,
	deviceidentity.Identity,
	certificate.Request,
	string,
) (Claim, error) {
	return store.claim, nil
}

func (store *recordingStore) Complete(
	_ context.Context,
	_ enrollment.Principal,
	claim Claim,
	certificate Certificate,
) (Response, error) {
	store.completed = true
	return responseFor(claim, certificate), nil
}

func (store *recordingStore) ListIssuingRenewals(context.Context, time.Time, int) ([]Claim, error) {
	return store.issuing, nil
}

type recordingIssuer struct {
	err      error
	requests []ca.IssuanceRequest
}

func (issuer *recordingIssuer) Issue(_ context.Context, request ca.IssuanceRequest) (ca.Certificate, error) {
	issuer.requests = append(issuer.requests, request)
	if issuer.err != nil {
		return ca.Certificate{}, issuer.err
	}
	return ca.Certificate{
		ChainPEM: "certificate-chain", SerialNumber: "02",
		NotBefore: time.Unix(1_000, 0), NotAfter: time.Unix(2_000, 0),
	}, nil
}

func TestRenewReturnsCompletedRetryWithoutIssuing(t *testing.T) {
	completed := Certificate{ChainPEM: "existing", SerialNumber: "02"}
	store := &recordingStore{claim: Claim{
		ID: "renewal-1", DeviceID: "device-1", PublicKeyFingerprint: "fingerprint",
		Completed: &completed,
	}}
	issuer := &recordingIssuer{}
	service := NewService(store, issuer)
	response, err := service.Renew(
		context.Background(),
		enrollment.Principal{Issuer: "issuer", Subject: "user"},
		deviceidentity.Identity{OrganizationID: "organization", DeviceID: "device", SerialNumber: "01"},
		signedCSR(t),
	)
	if err != nil {
		t.Fatal(err)
	}
	if response.RenewalID != "renewal-1" || response.Certificate.ChainPEM != "existing" || len(issuer.requests) != 0 {
		t.Fatalf("response = %#v, issuer requests = %d", response, len(issuer.requests))
	}
}

func TestRenewLeavesClaimWhenIssuanceIsAmbiguous(t *testing.T) {
	store := &recordingStore{claim: Claim{
		ID: "renewal-1", OrganizationID: "organization", DeviceID: "device",
		CSRDER: []byte("csr"), StartedAt: time.Unix(1_000, 0),
	}}
	issuer := &recordingIssuer{err: errors.New("CA timeout")}
	service := NewService(store, issuer)
	_, err := service.Renew(
		context.Background(),
		enrollment.Principal{Issuer: "issuer", Subject: "user"},
		deviceidentity.Identity{OrganizationID: "organization", DeviceID: "device", SerialNumber: "01"},
		signedCSR(t),
	)
	if !errors.Is(err, ErrIssuanceFailed) || store.completed {
		t.Fatalf("error = %v, completed = %v", err, store.completed)
	}
}

func TestReconcileRetriesStableClaim(t *testing.T) {
	startedAt := time.Unix(1_000, 0)
	claim := Claim{
		ID: "renewal-1", OrganizationID: "organization", OrganizationIssuer: "issuer",
		DeviceID: "device", CSRDER: []byte("csr"), StartedAt: startedAt,
	}
	store := &recordingStore{issuing: []Claim{claim}}
	issuer := &recordingIssuer{}
	completed, err := NewService(store, issuer).Reconcile(context.Background(), time.Now(), 10)
	if err != nil {
		t.Fatal(err)
	}
	if completed != 1 || !store.completed || len(issuer.requests) != 1 ||
		issuer.requests[0].ID != claim.ID || !issuer.requests[0].IssuedAt.Equal(startedAt) {
		t.Fatalf("completed = %d, store = %#v, requests = %#v", completed, store, issuer.requests)
	}
}

func TestRecoveryVerifiesEnrolledKeyBeforeIssuing(t *testing.T) {
	key, certificatePEM := recoveryIdentity(t)
	completed := Certificate{ChainPEM: "existing", SerialNumber: "02"}
	store := &recordingStore{
		recoveryChallenge: RecoveryChallenge{CertificatePEM: certificatePEM},
		recoveryClaim: Claim{
			ID: "renewal-1", DeviceID: "device-1", PublicKeyFingerprint: "fingerprint",
			Completed: &completed,
		},
	}
	service := NewService(store, &recordingIssuer{})
	challenge, err := service.CreateRecoveryChallenge(
		context.Background(), enrollment.Principal{Issuer: "issuer", Subject: "user"},
		"device-1", "01", signedCSR(t),
	)
	if err != nil {
		t.Fatal(err)
	}
	digest := sha256.Sum256(RecoveryMessage(store.recoveryChallenge))
	proof, err := ecdsa.SignASN1(rand.Reader, key, digest[:])
	if err != nil {
		t.Fatal(err)
	}
	response, err := service.Recover(
		context.Background(), enrollment.Principal{Issuer: "issuer", Subject: "user"},
		challenge.ChallengeID, base64.RawURLEncoding.EncodeToString(proof),
	)
	if err != nil {
		t.Fatal(err)
	}
	if response.RenewalID != "renewal-1" || response.Certificate.ChainPEM != "existing" {
		t.Fatalf("response = %#v", response)
	}
}

func signedCSR(t *testing.T) string {
	t.Helper()
	key, err := ecdsa.GenerateKey(elliptic.P256(), rand.Reader)
	if err != nil {
		t.Fatal(err)
	}
	der, err := x509.CreateCertificateRequest(rand.Reader, &x509.CertificateRequest{}, key)
	if err != nil {
		t.Fatal(err)
	}
	return string(pem.EncodeToMemory(&pem.Block{Type: "CERTIFICATE REQUEST", Bytes: der}))
}
