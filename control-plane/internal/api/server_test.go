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

func TestEnrollmentUsesAuthenticatedPrincipal(t *testing.T) {
	store := &recordingStore{}
	handler := NewServer(
		testAuthenticator{principal: enrollment.Principal{Issuer: "https://issuer.example/", Subject: "user-1"}},
		enrollment.NewService(store),
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
	service := enrollment.NewService(&recordingStore{})
	unauthenticated := NewServer(testAuthenticator{err: errors.New("invalid token")}, service)
	response := httptest.NewRecorder()
	unauthenticated.ServeHTTP(response, httptest.NewRequest(http.MethodPost, "/v1/enrollments", bytes.NewBufferString(`{}`)))
	if response.Code != http.StatusUnauthorized {
		t.Fatalf("unauthenticated status = %d", response.Code)
	}

	authenticated := NewServer(testAuthenticator{principal: enrollment.Principal{Issuer: "issuer", Subject: "subject"}}, service)
	response = httptest.NewRecorder()
	authenticated.ServeHTTP(response, httptest.NewRequest(http.MethodPost, "/v1/enrollments", bytes.NewBufferString(`{"csr":"value","subject":"attacker"}`)))
	if response.Code != http.StatusBadRequest {
		t.Fatalf("unknown identity field status = %d", response.Code)
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
