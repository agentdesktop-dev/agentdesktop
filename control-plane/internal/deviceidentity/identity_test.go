package deviceidentity

import (
	"crypto/tls"
	"crypto/x509"
	"math/big"
	"net/http"
	"net/url"
	"testing"
)

const (
	organizationID = "11111111-1111-4111-8111-111111111111"
	userID         = "33333333-3333-4333-8333-333333333333"
	deviceID       = "22222222-2222-4222-8222-222222222222"
	trustDomain    = "devices.example.com"
)

func TestFromRequestExtractsVerifiedSPIFFEIdentity(t *testing.T) {
	request := verifiedRequest(t, "spiffe://"+trustDomain+"/ns/"+organizationID+"/sa/user."+userID+".device."+deviceID)
	identity, err := FromRequest(request, trustDomain)
	if err != nil {
		t.Fatal(err)
	}
	if identity.OrganizationID != organizationID || identity.UserID != userID || identity.DeviceID != deviceID || identity.SerialNumber != "2a" {
		t.Fatalf("identity = %#v", identity)
	}
}

func TestFromRequestRejectsUnverifiedOrAmbiguousIdentity(t *testing.T) {
	identityURI, err := url.Parse("spiffe://" + trustDomain + "/ns/" + organizationID + "/sa/user." + userID + ".device." + deviceID)
	if err != nil {
		t.Fatal(err)
	}
	leaf := &x509.Certificate{SerialNumber: big.NewInt(42), URIs: []*url.URL{identityURI}}
	request := &http.Request{TLS: &tls.ConnectionState{PeerCertificates: []*x509.Certificate{leaf}}}
	if _, err := FromRequest(request, trustDomain); err == nil {
		t.Fatal("unverified peer certificate was accepted")
	}
	request.TLS.VerifiedChains = [][]*x509.Certificate{{leaf}}
	leaf.URIs = append(leaf.URIs, identityURI)
	if _, err := FromRequest(request, trustDomain); err == nil {
		t.Fatal("certificate with multiple identities was accepted")
	}
}

func TestFromRequestRejectsWrongDomainAndMalformedIdentifiers(t *testing.T) {
	request := verifiedRequest(t, "spiffe://other.example.com/ns/"+organizationID+"/sa/user."+userID+".device."+deviceID)
	if _, err := FromRequest(request, trustDomain); err == nil {
		t.Fatal("foreign trust domain was accepted")
	}
	request = verifiedRequest(t, "spiffe://"+trustDomain+"/ns/not-a-uuid/sa/user."+userID+".device."+deviceID)
	if _, err := FromRequest(request, trustDomain); err == nil {
		t.Fatal("malformed organization identifier was accepted")
	}
}

func verifiedRequest(t *testing.T, identity string) *http.Request {
	t.Helper()
	identityURI, err := url.Parse(identity)
	if err != nil {
		t.Fatal(err)
	}
	leaf := &x509.Certificate{SerialNumber: big.NewInt(42), URIs: []*url.URL{identityURI}}
	return &http.Request{TLS: &tls.ConnectionState{
		PeerCertificates: []*x509.Certificate{leaf},
		VerifiedChains:   [][]*x509.Certificate{{leaf}},
	}}
}
