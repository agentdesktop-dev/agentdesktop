package ca

import (
	"crypto/ecdsa"
	"crypto/elliptic"
	"crypto/rand"
	"crypto/x509"
	"crypto/x509/pkix"
	"encoding/pem"
	"math/big"
	"os"
	"path/filepath"
	"testing"
	"time"
)

func TestLoadPKCS11SignerRejectsBroadConfigurationPermissions(t *testing.T) {
	path := filepath.Join(t.TempDir(), "pkcs11.json")
	if err := os.WriteFile(path, []byte(`{}`), 0o644); err != nil {
		t.Fatal(err)
	}
	if _, err := LoadPKCS11Signer(path, "01"); err == nil || err.Error() != "PKCS#11 configuration must be a regular owner-only file" {
		t.Fatalf("error = %v", err)
	}
}

func TestPKCS11SignerIssuesAuthorityControlledCertificate(t *testing.T) {
	configPath := os.Getenv("TEST_PKCS11_CONFIG_PATH")
	keyID := os.Getenv("TEST_PKCS11_KEY_ID")
	if configPath == "" || keyID == "" {
		t.Skip("TEST_PKCS11_CONFIG_PATH and TEST_PKCS11_KEY_ID are not set")
	}
	signer, err := LoadPKCS11Signer(configPath, keyID)
	if err != nil {
		t.Fatal(err)
	}
	defer signer.Close()
	now := time.Now().UTC()
	template := &x509.Certificate{
		SerialNumber: big.NewInt(1), Subject: pkix.Name{CommonName: "Test PKCS11 CA"},
		NotBefore: now.Add(-time.Minute), NotAfter: now.Add(24 * time.Hour),
		IsCA: true, BasicConstraintsValid: true,
		KeyUsage: x509.KeyUsageCertSign | x509.KeyUsageCRLSign,
	}
	der, err := x509.CreateCertificate(rand.Reader, template, template, signer.Public(), signer)
	if err != nil {
		t.Fatal(err)
	}
	certificate, err := x509.ParseCertificate(der)
	if err != nil {
		t.Fatal(err)
	}
	certificatePath := filepath.Join(t.TempDir(), "ca.pem")
	certificatePEM := pem.EncodeToMemory(&pem.Block{Type: "CERTIFICATE", Bytes: der})
	if err := os.WriteFile(certificatePath, certificatePEM, 0o600); err != nil {
		t.Fatal(err)
	}
	issuer, err := LoadX509IssuerWithSigner(certificatePath, signer, "devices.example", time.Hour)
	if err != nil {
		t.Fatal(err)
	}
	clientKey, err := ecdsa.GenerateKey(elliptic.P256(), rand.Reader)
	if err != nil {
		t.Fatal(err)
	}
	csrDER, err := x509.CreateCertificateRequest(rand.Reader, &x509.CertificateRequest{}, clientKey)
	if err != nil {
		t.Fatal(err)
	}
	request := IssuanceRequest{
		ID: "pkcs11-issuance", CSRDER: csrDER, IssuedAt: now,
		Identity: Identity{OrganizationID: "organization", DeviceID: "device"},
	}
	first, err := issuer.Issue(t.Context(), request)
	if err != nil {
		t.Fatal(err)
	}
	second, err := issuer.Issue(t.Context(), request)
	if err != nil {
		t.Fatal(err)
	}
	if first.SerialNumber != second.SerialNumber || first.NotAfter != second.NotAfter {
		t.Fatalf("retry changed certificate identity: first=%#v second=%#v", first, second)
	}
	block, _ := pem.Decode([]byte(first.ChainPEM))
	leaf, err := x509.ParseCertificate(block.Bytes)
	if err != nil {
		t.Fatal(err)
	}
	if err := leaf.CheckSignatureFrom(certificate); err != nil {
		t.Fatalf("leaf was not signed by PKCS11 CA: %v", err)
	}
	if len(leaf.URIs) != 1 || leaf.URIs[0].String() != "spiffe://devices.example/organization/organization/device/device" {
		t.Fatalf("leaf URI = %v", leaf.URIs)
	}
}
