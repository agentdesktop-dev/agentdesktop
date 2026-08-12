package enrollment

import (
	"context"
	"errors"
	"strings"
	"testing"
	"time"

	"github.com/agentdesktop-dev/agentdesktop/control-plane/internal/ca"
	"github.com/agentdesktop-dev/agentdesktop/control-plane/internal/certificate"
)

type approvalStore struct {
	completed bool
	issuance  Issuance
	issuing   []Issuance
}

func (store *approvalStore) CreatePending(context.Context, Principal, certificate.Request, string, string) (Enrollment, error) {
	return Enrollment{}, nil
}

func TestNormalizeDeviceName(t *testing.T) {
	if value, err := normalizeDeviceName("  workstation-7  "); err != nil || value != "workstation-7" {
		t.Fatalf("normalized device name = %q, %v", value, err)
	}
	for _, value := range []string{"line\nbreak", strings.Repeat("x", maxDeviceNameLength+1)} {
		if _, err := normalizeDeviceName(value); !errors.Is(err, ErrInvalidDeviceName) {
			t.Fatalf("device name %q error = %v", value, err)
		}
	}
}

func (store *approvalStore) BeginIssuance(context.Context, Principal, string, string) (Issuance, error) {
	return store.issuance, nil
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

func (store *approvalStore) ListIssuing(context.Context, time.Time, int) ([]Issuance, error) {
	return store.issuing, nil
}

func (store *approvalStore) List(context.Context, Principal, string, int) ([]AdministrativeRecord, error) {
	return nil, nil
}

func (store *approvalStore) ListDevices(context.Context, Principal, int) ([]AdministrativeDevice, error) {
	return nil, nil
}

func (store *approvalStore) Summary(context.Context, Principal) (FleetSummary, error) {
	return FleetSummary{}, nil
}

func (store *approvalStore) Reject(context.Context, Principal, string) (AdministrativeRecord, error) {
	return AdministrativeRecord{}, nil
}

func (store *approvalStore) RevokeDevice(context.Context, Principal, string) (DeviceRevocation, error) {
	return DeviceRevocation{}, nil
}

type testIssuer struct {
	err error
}

func (issuer testIssuer) Issue(context.Context, ca.IssuanceRequest) (ca.Certificate, error) {
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
		DeviceID: "device-1", CSRDER: []byte("csr"), StartedAt: time.Unix(1_000, 0),
	}}
	service := NewService(store, testIssuer{})
	approval, err := service.Approve(context.Background(), Principal{Issuer: "issuer", Subject: "admin"}, "enrollment-1")
	if err != nil {
		t.Fatal(err)
	}
	if !store.completed || approval.Status != "approved" || approval.DeviceID != "device-1" {
		t.Fatalf("approval = %#v, store = %#v", approval, store)
	}
}

func TestApproveLeavesClaimForReconciliationWhenIssuerFails(t *testing.T) {
	store := &approvalStore{issuance: Issuance{EnrollmentID: "enrollment-1", StartedAt: time.Unix(1_000, 0)}}
	service := NewService(store, testIssuer{err: errors.New("CA unavailable")})
	if _, err := service.Approve(context.Background(), Principal{Issuer: "issuer", Subject: "admin"}, "enrollment-1"); err == nil {
		t.Fatal("approval succeeded with failed issuer")
	}
	if store.completed {
		t.Fatalf("store = %#v", store)
	}
}

func TestReconcileRetriesOriginalIssuanceWithoutAborting(t *testing.T) {
	issuance := Issuance{
		EnrollmentID: "enrollment-1", OrganizationID: "organization-1",
		OrganizationIssuer: "issuer", DeviceID: "device-1",
		CSRDER: []byte("csr"), StartedAt: time.Unix(1_000, 0),
	}
	store := &approvalStore{issuing: []Issuance{issuance}}
	service := NewService(store, testIssuer{})
	completed, err := service.Reconcile(context.Background(), time.Unix(2_000, 0), 10)
	if err != nil {
		t.Fatal(err)
	}
	if completed != 1 || !store.completed {
		t.Fatalf("completed = %d, store = %#v", completed, store)
	}

	store = &approvalStore{issuing: []Issuance{issuance}}
	service = NewService(store, testIssuer{err: errors.New("CA unavailable")})
	completed, err = service.Reconcile(context.Background(), time.Unix(2_000, 0), 10)
	if completed != 0 || !errors.Is(err, ErrIssuanceFailed) {
		t.Fatalf("completed = %d, error = %v, store = %#v", completed, err, store)
	}
}
