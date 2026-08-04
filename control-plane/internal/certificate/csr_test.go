package certificate

import (
	"crypto/ecdsa"
	"crypto/elliptic"
	"crypto/rand"
	"crypto/rsa"
	"crypto/x509"
	"crypto/x509/pkix"
	"encoding/pem"
	"errors"
	"testing"
)

func TestParseRequestAcceptsSignedP256CSR(t *testing.T) {
	key, err := ecdsa.GenerateKey(elliptic.P256(), rand.Reader)
	if err != nil {
		t.Fatal(err)
	}
	encoded := encodeCSR(t, key)

	request, err := ParseRequest(encoded)
	if err != nil {
		t.Fatal(err)
	}
	if len(request.DER) == 0 || request.PublicKeyFingerprint == "" {
		t.Fatal("validated CSR is missing durable key identity")
	}
	second, err := ParseRequest(encoded)
	if err != nil {
		t.Fatal(err)
	}
	if second.PublicKeyFingerprint != request.PublicKeyFingerprint {
		t.Fatal("public key fingerprint is not deterministic")
	}
}

func TestParseRequestRejectsUnsupportedKeyAndTrailingData(t *testing.T) {
	rsaKey, err := rsa.GenerateKey(rand.Reader, 2048)
	if err != nil {
		t.Fatal(err)
	}
	if _, err := ParseRequest(encodeCSR(t, rsaKey)); !errors.Is(err, ErrInvalidCSR) {
		t.Fatalf("RSA CSR error = %v, want ErrInvalidCSR", err)
	}

	ecdsaKey, err := ecdsa.GenerateKey(elliptic.P256(), rand.Reader)
	if err != nil {
		t.Fatal(err)
	}
	if _, err := ParseRequest(encodeCSR(t, ecdsaKey) + "trailing"); !errors.Is(err, ErrInvalidCSR) {
		t.Fatalf("trailing data error = %v, want ErrInvalidCSR", err)
	}
}

func encodeCSR(t *testing.T, key any) string {
	t.Helper()
	der, err := x509.CreateCertificateRequest(rand.Reader, &x509.CertificateRequest{
		Subject: pkix.Name{CommonName: "ignored-client-value"},
	}, key)
	if err != nil {
		t.Fatal(err)
	}
	return string(pem.EncodeToMemory(&pem.Block{Type: "CERTIFICATE REQUEST", Bytes: der}))
}
