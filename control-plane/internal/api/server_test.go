package api

import (
	"bytes"
	"context"
	"crypto/ecdsa"
	"crypto/elliptic"
	"crypto/rand"
	"crypto/tls"
	"crypto/x509"
	"encoding/json"
	"encoding/pem"
	"errors"
	"math/big"
	"net/http"
	"net/http/httptest"
	"net/url"
	"testing"
	"time"

	"github.com/agentdesktop-dev/agentdesktop/control-plane/internal/ca"
	"github.com/agentdesktop-dev/agentdesktop/control-plane/internal/certificate"
	"github.com/agentdesktop-dev/agentdesktop/control-plane/internal/deviceidentity"
	"github.com/agentdesktop-dev/agentdesktop/control-plane/internal/enrollment"
	"github.com/agentdesktop-dev/agentdesktop/control-plane/internal/renewal"
)

type testAuthenticator struct {
	principal enrollment.Principal
	err       error
}

func (authenticator testAuthenticator) Authenticate(*http.Request) (enrollment.Principal, error) {
	return authenticator.principal, authenticator.err
}

type recordingStore struct {
	principal    enrollment.Principal
	issuance     enrollment.Issuance
	status       enrollment.Status
	getErr       error
	adminRecords []enrollment.AdministrativeRecord
}

func (store *recordingStore) Get(
	_ context.Context,
	principal enrollment.Principal,
	_ string,
) (enrollment.Status, error) {
	store.principal = principal
	return store.status, store.getErr
}

func (store *recordingStore) ListIssuing(context.Context, time.Time, int) ([]enrollment.Issuance, error) {
	return nil, nil
}

func (store *recordingStore) List(context.Context, enrollment.Principal, string, int) ([]enrollment.AdministrativeRecord, error) {
	return store.adminRecords, nil
}

func (store *recordingStore) Reject(_ context.Context, _ enrollment.Principal, id string) (enrollment.AdministrativeRecord, error) {
	return enrollment.AdministrativeRecord{EnrollmentID: id, Status: "rejected"}, nil
}

func (store *recordingStore) RevokeDevice(_ context.Context, _ enrollment.Principal, id string) (enrollment.DeviceRevocation, error) {
	return enrollment.DeviceRevocation{DeviceID: id, Status: "revoked"}, nil
}

func (store *recordingStore) CreatePending(
	_ context.Context,
	principal enrollment.Principal,
	request certificate.Request,
	id string,
) (enrollment.Enrollment, error) {
	store.principal = principal
	return enrollment.Enrollment{
		ID: id, Status: "pending", Issuer: principal.Issuer, Subject: principal.Subject,
		PublicKeyFingerprint: request.PublicKeyFingerprint,
	}, nil
}

func (store *recordingStore) BeginIssuance(
	_ context.Context,
	_ enrollment.Principal,
	enrollmentID string,
	deviceID string,
) (enrollment.Issuance, error) {
	store.issuance.EnrollmentID = enrollmentID
	store.issuance.DeviceID = deviceID
	return store.issuance, nil
}

func (store *recordingStore) CompleteIssuance(
	_ context.Context,
	_ enrollment.Principal,
	issuance enrollment.Issuance,
	certificate enrollment.IssuedCertificate,
) (enrollment.Approval, error) {
	return enrollment.Approval{
		EnrollmentID:   issuance.EnrollmentID,
		Status:         "approved",
		DeviceID:       issuance.DeviceID,
		CertificatePEM: certificate.ChainPEM,
		SerialNumber:   certificate.SerialNumber,
		NotBefore:      certificate.NotBefore,
		NotAfter:       certificate.NotAfter,
	}, nil
}

type apiIssuer struct{}

func (apiIssuer) Issue(context.Context, ca.IssuanceRequest) (ca.Certificate, error) {
	return ca.Certificate{
		ChainPEM:     "certificate-chain",
		SerialNumber: "01",
		NotBefore:    time.Unix(1_000, 0),
		NotAfter:     time.Unix(2_000, 0),
	}, nil
}

type renewalStore struct {
	principal enrollment.Principal
	device    deviceidentity.Identity
	request   certificate.Request
}

func (store *renewalStore) Begin(
	_ context.Context,
	principal enrollment.Principal,
	device deviceidentity.Identity,
	request certificate.Request,
	_ string,
) (renewal.Claim, error) {
	store.principal = principal
	store.device = device
	store.request = request
	return renewal.Claim{
		ID:                   "33333333-3333-4333-8333-333333333333",
		OrganizationID:       device.OrganizationID,
		DeviceID:             device.DeviceID,
		CSRDER:               request.DER,
		PublicKeyFingerprint: request.PublicKeyFingerprint,
		StartedAt:            time.Unix(1_000, 0),
	}, nil
}

func (store *renewalStore) Complete(
	_ context.Context,
	_ enrollment.Principal,
	claim renewal.Claim,
	issued renewal.Certificate,
) (renewal.Response, error) {
	return renewal.Response{
		RenewalID: claim.ID, Status: "approved", DeviceID: claim.DeviceID,
		PublicKeyFingerprint: claim.PublicKeyFingerprint, Certificate: issued,
	}, nil
}

func (store *renewalStore) ListIssuingRenewals(context.Context, time.Time, int) ([]renewal.Claim, error) {
	return nil, nil
}

func TestEnrollmentUsesAuthenticatedPrincipal(t *testing.T) {
	store := &recordingStore{}
	handler := NewServer(
		testAuthenticator{principal: enrollment.Principal{Issuer: "https://issuer.example/", Subject: "user-1"}},
		testAuthenticator{principal: enrollment.Principal{Issuer: "https://issuer.example/", Subject: "admin-1"}},
		enrollment.NewService(store, apiIssuer{}),
	)
	body, err := json.Marshal(map[string]string{"csr": signedCSR(t)})
	if err != nil {
		t.Fatal(err)
	}
	request := httptest.NewRequest(http.MethodPost, "/v1/enrollments", bytes.NewReader(body))
	response := httptest.NewRecorder()
	handler.ServeHTTP(response, request)

	if response.Code != http.StatusAccepted {
		t.Fatalf("status = %d, body = %s", response.Code, response.Body.String())
	}
	if store.principal.Subject != "user-1" || store.principal.Issuer != "https://issuer.example/" {
		t.Fatalf("stored principal = %#v", store.principal)
	}
}

func TestRenewalRequiresOAuthAndVerifiedDeviceCertificate(t *testing.T) {
	const (
		organizationID = "11111111-1111-4111-8111-111111111111"
		deviceID       = "22222222-2222-4222-8222-222222222222"
		trustDomain    = "devices.example.com"
	)
	owner := enrollment.Principal{Issuer: "https://issuer.example/", Subject: "user-1"}
	store := &renewalStore{}
	handler := NewServer(
		testAuthenticator{principal: owner},
		testAuthenticator{},
		enrollment.NewService(&recordingStore{}, apiIssuer{}),
		WithRenewal(testAuthenticator{principal: owner}, renewal.NewService(store, apiIssuer{}), trustDomain),
	)
	body, err := json.Marshal(map[string]string{"csr": signedCSR(t)})
	if err != nil {
		t.Fatal(err)
	}
	request := httptest.NewRequest(http.MethodPost, "/v1/renewals", bytes.NewReader(body))
	response := httptest.NewRecorder()
	handler.ServeHTTP(response, request)
	if response.Code != http.StatusUnauthorized {
		t.Fatalf("renewal without device certificate status = %d", response.Code)
	}

	identityURI, err := url.Parse("spiffe://" + trustDomain + "/organization/" + organizationID + "/device/" + deviceID)
	if err != nil {
		t.Fatal(err)
	}
	leaf := &x509.Certificate{SerialNumber: big.NewInt(1), URIs: []*url.URL{identityURI}}
	request = httptest.NewRequest(http.MethodPost, "/v1/renewals", bytes.NewReader(body))
	request.TLS = &tls.ConnectionState{
		PeerCertificates: []*x509.Certificate{leaf},
		VerifiedChains:   [][]*x509.Certificate{{leaf}},
	}
	response = httptest.NewRecorder()
	handler.ServeHTTP(response, request)
	if response.Code != http.StatusOK {
		t.Fatalf("renewal status = %d, body = %s", response.Code, response.Body.String())
	}
	if store.principal != owner || store.device.OrganizationID != organizationID ||
		store.device.DeviceID != deviceID || store.device.SerialNumber != "1" ||
		store.request.PublicKeyFingerprint == "" {
		t.Fatalf("renewal identity = %#v, principal = %#v", store.device, store.principal)
	}
}

func TestEnrollmentRejectsUnauthenticatedAndUnknownFields(t *testing.T) {
	service := enrollment.NewService(&recordingStore{}, apiIssuer{})
	unauthenticated := NewServer(testAuthenticator{err: errors.New("invalid token")}, testAuthenticator{}, service)
	response := httptest.NewRecorder()
	unauthenticated.ServeHTTP(response, httptest.NewRequest(http.MethodPost, "/v1/enrollments", bytes.NewBufferString(`{}`)))
	if response.Code != http.StatusUnauthorized {
		t.Fatalf("unauthenticated status = %d", response.Code)
	}

	authenticated := NewServer(testAuthenticator{principal: enrollment.Principal{Issuer: "issuer", Subject: "subject"}}, testAuthenticator{}, service)
	response = httptest.NewRecorder()
	authenticated.ServeHTTP(response, httptest.NewRequest(http.MethodPost, "/v1/enrollments", bytes.NewBufferString(`{"csr":"value","subject":"attacker"}`)))
	if response.Code != http.StatusBadRequest {
		t.Fatalf("unknown identity field status = %d", response.Code)
	}
}

func TestApprovalRequiresAdministratorAndReturnsCertificate(t *testing.T) {
	store := &recordingStore{issuance: enrollment.Issuance{
		OrganizationID: "organization-1", CSRDER: []byte("csr"), StartedAt: time.Unix(1_000, 0),
	}}
	administrator := testAuthenticator{principal: enrollment.Principal{Issuer: "https://issuer.example/", Subject: "admin-1"}}
	handler := NewServer(testAuthenticator{}, administrator, enrollment.NewService(store, apiIssuer{}))
	response := httptest.NewRecorder()
	handler.ServeHTTP(response, httptest.NewRequest(http.MethodPost, "/v1/admin/enrollments/enrollment-1/approve", nil))
	if response.Code != http.StatusOK {
		t.Fatalf("status = %d, body = %s", response.Code, response.Body.String())
	}
	var approval enrollment.Approval
	if err := json.Unmarshal(response.Body.Bytes(), &approval); err != nil {
		t.Fatal(err)
	}
	if approval.EnrollmentID != "enrollment-1" || approval.CertificatePEM != "certificate-chain" {
		t.Fatalf("approval = %#v", approval)
	}

	unauthorized := NewServer(testAuthenticator{}, testAuthenticator{err: errors.New("not admin")}, enrollment.NewService(store, apiIssuer{}))
	response = httptest.NewRecorder()
	unauthorized.ServeHTTP(response, httptest.NewRequest(http.MethodPost, "/v1/admin/enrollments/enrollment-1/approve", nil))
	if response.Code != http.StatusUnauthorized {
		t.Fatalf("unauthorized status = %d", response.Code)
	}
}

func TestEnrollmentStatusUsesAuthenticatedOwnerAndReturnsCertificate(t *testing.T) {
	store := &recordingStore{status: enrollment.Status{
		EnrollmentID: "enrollment-1",
		Status:       "approved",
		Certificate: &enrollment.IssuedCertificate{
			ChainPEM: "certificate-chain",
		},
	}}
	owner := enrollment.Principal{Issuer: "https://issuer.example/", Subject: "user-1"}
	handler := NewServer(testAuthenticator{principal: owner}, testAuthenticator{}, enrollment.NewService(store, apiIssuer{}))
	response := httptest.NewRecorder()
	handler.ServeHTTP(response, httptest.NewRequest(http.MethodGet, "/v1/enrollments/enrollment-1", nil))
	if response.Code != http.StatusOK {
		t.Fatalf("status = %d, body = %s", response.Code, response.Body.String())
	}
	var record enrollment.Status
	if err := json.Unmarshal(response.Body.Bytes(), &record); err != nil {
		t.Fatal(err)
	}
	if store.principal != owner || record.Certificate == nil || record.Certificate.ChainPEM != "certificate-chain" {
		t.Fatalf("record = %#v, principal = %#v", record, store.principal)
	}

	store.getErr = enrollment.ErrNotFound
	response = httptest.NewRecorder()
	handler.ServeHTTP(response, httptest.NewRequest(http.MethodGet, "/v1/enrollments/foreign", nil))
	if response.Code != http.StatusNotFound {
		t.Fatalf("foreign enrollment status = %d", response.Code)
	}
}

func TestAdministratorListsAndRejectsPendingEnrollment(t *testing.T) {
	store := &recordingStore{adminRecords: []enrollment.AdministrativeRecord{{
		EnrollmentID: "enrollment-1",
		Status:       "pending",
		Subject:      "user-1",
	}}}
	administrator := testAuthenticator{principal: enrollment.Principal{
		Issuer: "https://issuer.example/", Subject: "admin-1",
	}}
	handler := NewServer(testAuthenticator{}, administrator, enrollment.NewService(store, apiIssuer{}))

	response := httptest.NewRecorder()
	handler.ServeHTTP(response, httptest.NewRequest(http.MethodGet, "/v1/admin/enrollments", nil))
	if response.Code != http.StatusOK || !bytes.Contains(response.Body.Bytes(), []byte(`"enrollment_id":"enrollment-1"`)) {
		t.Fatalf("list status = %d, body = %s", response.Code, response.Body.String())
	}

	response = httptest.NewRecorder()
	handler.ServeHTTP(response, httptest.NewRequest(http.MethodPost, "/v1/admin/enrollments/enrollment-1/reject", nil))
	if response.Code != http.StatusOK || !bytes.Contains(response.Body.Bytes(), []byte(`"status":"rejected"`)) {
		t.Fatalf("reject status = %d, body = %s", response.Code, response.Body.String())
	}

	response = httptest.NewRecorder()
	handler.ServeHTTP(response, httptest.NewRequest(http.MethodPost, "/v1/admin/devices/device-1/revoke", nil))
	if response.Code != http.StatusOK || !bytes.Contains(response.Body.Bytes(), []byte(`"status":"revoked"`)) {
		t.Fatalf("revoke device status = %d, body = %s", response.Code, response.Body.String())
	}

	response = httptest.NewRecorder()
	handler.ServeHTTP(response, httptest.NewRequest(http.MethodGet, "/v1/admin/enrollments?status=unknown", nil))
	if response.Code != http.StatusBadRequest {
		t.Fatalf("invalid filter status = %d", response.Code)
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
