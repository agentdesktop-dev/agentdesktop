package transport

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

func TestLoadServerTLSConfig(t *testing.T) {
	directory := t.TempDir()
	caCertificate, caKey := createCertificate(t, nil, nil, true, "enrollment-ca")
	serverCertificate, serverKey := createCertificate(t, caCertificate, caKey, false, "enrollment.example.com")
	certificatePath := writePEM(t, directory, "server.crt", "CERTIFICATE", serverCertificate.Raw, 0o644)
	keyDER, err := x509.MarshalPKCS8PrivateKey(serverKey)
	if err != nil {
		t.Fatal(err)
	}
	keyPath := writePEM(t, directory, "server.key", "PRIVATE KEY", keyDER, 0o600)
	caPath := writePEM(t, directory, "ca.crt", "CERTIFICATE", caCertificate.Raw, 0o644)

	configuration, err := LoadServerTLSConfig(certificatePath, keyPath, caPath)
	if err != nil {
		t.Fatal(err)
	}
	if configuration.MinVersion != 0x0304 || configuration.ClientAuth != 3 ||
		len(configuration.Certificates) != 1 || configuration.ClientCAs == nil {
		t.Fatalf("TLS configuration = %#v", configuration)
	}
}

func TestLoadServerTLSConfigRejectsUnprotectedKey(t *testing.T) {
	directory := t.TempDir()
	certificate, key := createCertificate(t, nil, nil, true, "server")
	certificatePath := writePEM(t, directory, "server.crt", "CERTIFICATE", certificate.Raw, 0o644)
	keyDER, err := x509.MarshalPKCS8PrivateKey(key)
	if err != nil {
		t.Fatal(err)
	}
	keyPath := writePEM(t, directory, "server.key", "PRIVATE KEY", keyDER, 0o644)
	caPath := writePEM(t, directory, "ca.crt", "CERTIFICATE", certificate.Raw, 0o644)
	if _, err := LoadServerTLSConfig(certificatePath, keyPath, caPath); err == nil {
		t.Fatal("unprotected server key was accepted")
	}
}

func createCertificate(
	t *testing.T,
	parent *x509.Certificate,
	parentKey *ecdsa.PrivateKey,
	isCA bool,
	commonName string,
) (*x509.Certificate, *ecdsa.PrivateKey) {
	t.Helper()
	key, err := ecdsa.GenerateKey(elliptic.P256(), rand.Reader)
	if err != nil {
		t.Fatal(err)
	}
	template := &x509.Certificate{
		SerialNumber:          big.NewInt(time.Now().UnixNano()),
		Subject:               pkix.Name{CommonName: commonName},
		NotBefore:             time.Now().Add(-time.Minute),
		NotAfter:              time.Now().Add(time.Hour),
		BasicConstraintsValid: true,
		IsCA:                  isCA,
		KeyUsage:              x509.KeyUsageDigitalSignature,
	}
	if isCA {
		template.KeyUsage |= x509.KeyUsageCertSign
	}
	if parent == nil {
		parent = template
		parentKey = key
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

func writePEM(t *testing.T, directory, name, blockType string, der []byte, mode os.FileMode) string {
	t.Helper()
	path := filepath.Join(directory, name)
	if err := os.WriteFile(path, pem.EncodeToMemory(&pem.Block{Type: blockType, Bytes: der}), mode); err != nil {
		t.Fatal(err)
	}
	return path
}
