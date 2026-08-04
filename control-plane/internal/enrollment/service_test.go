package enrollment

import (
	"context"
	"errors"
	"testing"
	"time"

	"github.com/agentdesktop-dev/agentdesktop/control-plane/internal/ca"
	"github.com/agentdesktop-dev/agentdesktop/control-plane/internal/certificate"
)

type approvalStore struct {
	aborted   bool
	completed bool
	issuance  Issuance
}

func (store *approvalStore) CreatePending(context.Context, Principal, certificate.Request, string) (Enrollment, error) {
	return Enrollment{}, nil
}

func (store *approvalStore) BeginIssuance(context.Context, Principal, string, string) (Issuance, error) {
	return store.issuance, nil
}

func (store *approvalStore) AbortIssuance(context.Context, Principal, Issuance) error {
	store.aborted = true
	return nil
}

func (store *approvalStore) CompleteIssuance(
	_ context.Context,
	_ Principal,
	issuance Issuance,
	certificate IssuedCertificate,
) (Approval, error) {
	store.completed = true
	return Approval{
		EnrollmentID:   issuance.EnrollmentID,
		Status:         "approved",
		DeviceID:       issuance.DeviceID,
		CertificatePEM: certificate.ChainPEM,
		SerialNumber:   certificate.SerialNumber,
		NotBefore:      certificate.NotBefore,
		NotAfter:       certificate.NotAfter,
	}, nil
}

func (store *approvalStore) Get(context.Context, Principal, string) (Status, error) {
	return Status{}, nil
}

type testIssuer struct {
	err error
}

func (issuer testIssuer) Issue(context.Context, ca.Identity, []byte) (ca.Certificate, error) {
	if issuer.err != nil {
		return ca.Certificate{}, issuer.err
	}
	return ca.Certificate{
		ChainPEM:     "certificate-chain",
		SerialNumber: "01",
		NotBefore:    time.Unix(1_000, 0),
		NotAfter:     time.Unix(2_000, 0),
	}, nil
}

func TestApproveCompletesClaimedIssuance(t *testing.T) {
	store := &approvalStore{issuance: Issuance{
		EnrollmentID: "enrollment-1", OrganizationID: "organization-1",
		DeviceID: "device-1", CSRDER: []byte("csr"),
	}}
	service := NewService(store, testIssuer{})
	approval, err := service.Approve(context.Background(), Principal{Issuer: "issuer", Subject: "admin"}, "enrollment-1")
	if err != nil {
		t.Fatal(err)
	}
	if !store.completed || store.aborted || approval.Status != "approved" || approval.DeviceID != "device-1" {
		t.Fatalf("approval = %#v, store = %#v", approval, store)
	}
}

func TestApproveAbortsClaimWhenIssuerFails(t *testing.T) {
	store := &approvalStore{issuance: Issuance{EnrollmentID: "enrollment-1"}}
	service := NewService(store, testIssuer{err: errors.New("CA unavailable")})
	if _, err := service.Approve(context.Background(), Principal{Issuer: "issuer", Subject: "admin"}, "enrollment-1"); err == nil {
		t.Fatal("approval succeeded with failed issuer")
	}
	if !store.aborted || store.completed {
		t.Fatalf("store = %#v", store)
	}
}
