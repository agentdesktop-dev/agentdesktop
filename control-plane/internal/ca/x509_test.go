package ca

import (
	"context"
	"crypto/ecdsa"
	"crypto/elliptic"
	"crypto/rand"
	"crypto/x509"
	"crypto/x509/pkix"
	"encoding/pem"
	"math/big"
	"testing"
	"time"
)

func TestX509IssuerUsesAuthorityControlledClientIdentity(t *testing.T) {
	issuerCertificate, issuerKey := testCA(t)
	issuer, err := NewX509Issuer(
		issuerCertificate,
		string(pem.EncodeToMemory(&pem.Block{Type: "CERTIFICATE", Bytes: issuerCertificate.Raw})),
		issuerKey,
		"devices.example.com",
		24*time.Hour,
	)
	if err != nil {
		t.Fatal(err)
	}
	clientKey, err := ecdsa.GenerateKey(elliptic.P256(), rand.Reader)
	if err != nil {
		t.Fatal(err)
	}
	csrDER, err := x509.CreateCertificateRequest(rand.Reader, &x509.CertificateRequest{
		Subject:  pkix.Name{CommonName: "attacker-controlled"},
		DNSNames: []string{"attacker.example"},
	}, clientKey)
	if err != nil {
		t.Fatal(err)
	}

	request := IssuanceRequest{
		ID:       "enrollment-1",
		CSRDER:   csrDER,
		IssuedAt: time.Unix(2_000_000_000, 0),
		Identity: Identity{
			OrganizationID: "organization-1",
			UserID:         "user-1",
			DeviceID:       "device-1",
		},
	}
	issued, err := issuer.Issue(context.Background(), request)
	if err != nil {
		t.Fatal(err)
	}
	block, _ := pem.Decode([]byte(issued.ChainPEM))
	certificate, err := x509.ParseCertificate(block.Bytes)
	if err != nil {
		t.Fatal(err)
	}
	if certificate.Subject.CommonName != "" || len(certificate.DNSNames) != 0 {
		t.Fatal("client-controlled CSR identity was copied into certificate")
	}
	if len(certificate.URIs) != 1 || certificate.URIs[0].String() != "spiffe://devices.example.com/ns/organization-1/sa/user.user-1.device.device-1" {
		t.Fatalf("certificate identity = %v", certificate.URIs)
	}
	if len(certificate.ExtKeyUsage) != 1 || certificate.ExtKeyUsage[0] != x509.ExtKeyUsageClientAuth {
		t.Fatalf("extended key usage = %v", certificate.ExtKeyUsage)
	}
	if err := certificate.CheckSignatureFrom(issuerCertificate); err != nil {
		t.Fatal(err)
	}
	retried, err := issuer.Issue(context.Background(), request)
	if err != nil {
		t.Fatal(err)
	}
	if retried.SerialNumber != issued.SerialNumber || !retried.NotBefore.Equal(issued.NotBefore) || !retried.NotAfter.Equal(issued.NotAfter) {
		t.Fatalf("retry changed certificate identity or validity: first=%#v retry=%#v", issued, retried)
	}
}

func testCA(t *testing.T) (*x509.Certificate, *ecdsa.PrivateKey) {
	t.Helper()
	key, err := ecdsa.GenerateKey(elliptic.P256(), rand.Reader)
	if err != nil {
		t.Fatal(err)
	}
	now := time.Unix(2_000_000_000, 0)
	template := &x509.Certificate{
		SerialNumber:          big.NewInt(1),
		Subject:               pkix.Name{CommonName: "Test Enrollment CA"},
		NotBefore:             now.Add(-time.Hour),
		NotAfter:              now.Add(30 * 24 * time.Hour),
		IsCA:                  true,
		BasicConstraintsValid: true,
		KeyUsage:              x509.KeyUsageCertSign | x509.KeyUsageCRLSign,
	}
	der, err := x509.CreateCertificate(rand.Reader, template, template, &key.PublicKey, key)
	if err != nil {
		t.Fatal(err)
	}
	certificate, err := x509.ParseCertificate(der)
	if err != nil {
		t.Fatal(err)
	}
	return certificate, key
}
