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
	"math/big"
	"net/http"
	"net/http/httptest"
	"net/url"
	"testing"
	"time"

	"github.com/agentdesktop-dev/agentdesktop/control-plane/internal/deviceauthorization"
	"github.com/agentdesktop-dev/agentdesktop/control-plane/internal/deviceidentity"
	"github.com/agentdesktop-dev/agentdesktop/control-plane/internal/enrollment"
)

type authorizationStore struct {
	principal enrollment.Principal
	device    deviceidentity.Identity
}

func (store *authorizationStore) AuthorizeDevice(
	_ context.Context,
	principal enrollment.Principal,
	device deviceidentity.Identity,
) error {
	store.principal = principal
	store.device = device
	return nil
}

func TestGatewayAuthorizesVerifiedOwnerAndDevice(t *testing.T) {
	const (
		organizationID = "11111111-1111-4111-8111-111111111111"
		deviceID       = "22222222-2222-4222-8222-222222222222"
		trustDomain    = "devices.example.com"
	)
	caCertificate, caKey := authorizationCertificate(t, nil, nil, nil, true)
	deviceURI := mustURL(t, "spiffe://"+trustDomain+"/organization/"+organizationID+"/device/"+deviceID)
	deviceCertificate, _ := authorizationCertificate(t, caCertificate, caKey, deviceURI, false)
	roots := x509.NewCertPool()
	roots.AddCert(caCertificate)
	gatewayIdentity := mustURL(t, "spiffe://services.example.com/service/agentgateway")
	gatewayCertificate := &x509.Certificate{URIs: []*url.URL{gatewayIdentity}}
	store := &authorizationStore{}
	handler := NewServer(
		testAuthenticator{}, testAuthenticator{}, enrollment.NewService(&recordingStore{}, apiIssuer{}),
		WithDeviceAuthorization(deviceauthorization.NewService(store), trustDomain, roots, gatewayIdentity),
	)
	body, err := json.Marshal(map[string]string{
		"certificate_pem": string(pem.EncodeToMemory(&pem.Block{Type: "CERTIFICATE", Bytes: deviceCertificate.Raw})),
		"issuer":          "https://issuer.example/",
		"subject":         "user-1",
	})
	if err != nil {
		t.Fatal(err)
	}
	request := httptest.NewRequest(http.MethodPost, "/v1/gateway/device-authorizations", bytes.NewReader(body))
	request.TLS = &tls.ConnectionState{VerifiedChains: [][]*x509.Certificate{{gatewayCertificate}}}
	response := httptest.NewRecorder()
	handler.ServeHTTP(response, request)
	if response.Code != http.StatusOK {
		t.Fatalf("status = %d, body = %s", response.Code, response.Body.String())
	}
	if store.principal.Issuer != "https://issuer.example/" || store.principal.Subject != "user-1" ||
		store.device.OrganizationID != organizationID || store.device.DeviceID != deviceID {
		t.Fatalf("principal = %#v, device = %#v", store.principal, store.device)
	}

	request = httptest.NewRequest(http.MethodPost, "/v1/gateway/device-authorizations", bytes.NewReader(body))
	response = httptest.NewRecorder()
	handler.ServeHTTP(response, request)
	if response.Code != http.StatusUnauthorized {
		t.Fatalf("unauthenticated Gateway status = %d", response.Code)
	}
}

func authorizationCertificate(
	t *testing.T,
	parent *x509.Certificate,
	parentKey *ecdsa.PrivateKey,
	identity *url.URL,
	isCA bool,
) (*x509.Certificate, *ecdsa.PrivateKey) {
	t.Helper()
	key, err := ecdsa.GenerateKey(elliptic.P256(), rand.Reader)
	if err != nil {
		t.Fatal(err)
	}
	template := &x509.Certificate{
		SerialNumber: big.NewInt(42), NotBefore: time.Now().Add(-time.Minute),
		NotAfter: time.Now().Add(time.Hour), BasicConstraintsValid: true,
		IsCA: isCA, KeyUsage: x509.KeyUsageDigitalSignature,
	}
	if isCA {
		template.KeyUsage |= x509.KeyUsageCertSign
	} else {
		template.ExtKeyUsage = []x509.ExtKeyUsage{x509.ExtKeyUsageClientAuth}
		template.URIs = []*url.URL{identity}
	}
	if parent == nil {
		parent, parentKey = template, key
	}
	der, err := x509.CreateCertificate(rand.Reader, template, parent, &key.PublicKey, parentKey)
	if err != nil {
		t.Fatal(err)
	}
	certificate, err := x509.ParseCertificate(der)
	if err != nil {
		t.Fatal(err)
	}
	return certificate, key
}

func mustURL(t *testing.T, value string) *url.URL {
	t.Helper()
	parsed, err := url.Parse(value)
	if err != nil {
		t.Fatal(err)
	}
	return parsed
}
