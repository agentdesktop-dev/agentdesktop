package api

import (
	"bytes"
	"context"
	"crypto/ecdsa"
	"crypto/elliptic"
	"crypto/rand"
	"crypto/x509"
	"encoding/json"
	"encoding/pem"
	"errors"
	"net/http"
	"net/http/httptest"
	"testing"
	"time"

	"github.com/agentdesktop-dev/agentdesktop/control-plane/internal/ca"
	"github.com/agentdesktop-dev/agentdesktop/control-plane/internal/certificate"
	"github.com/agentdesktop-dev/agentdesktop/control-plane/internal/enrollment"
)

type testAuthenticator struct {
	principal enrollment.Principal
	err       error
}

func (authenticator testAuthenticator) Authenticate(*http.Request) (enrollment.Principal, error) {
	return authenticator.principal, authenticator.err
}

type recordingStore struct {
	principal enrollment.Principal
	issuance  enrollment.Issuance
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

func (store *recordingStore) AbortIssuance(context.Context, enrollment.Principal, enrollment.Issuance) error {
	return nil
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

func (apiIssuer) Issue(context.Context, ca.Identity, []byte) (ca.Certificate, error) {
	return ca.Certificate{
		ChainPEM:     "certificate-chain",
		SerialNumber: "01",
		NotBefore:    time.Unix(1_000, 0),
		NotAfter:     time.Unix(2_000, 0),
	}, nil
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
	store := &recordingStore{issuance: enrollment.Issuance{OrganizationID: "organization-1", CSRDER: []byte("csr")}}
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
