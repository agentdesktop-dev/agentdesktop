package renewal

import (
	"crypto/ecdsa"
	"crypto/elliptic"
	"crypto/rand"
	"crypto/sha256"
	"crypto/x509"
	"crypto/x509/pkix"
	"encoding/base64"
	"encoding/pem"
	"errors"
	"math/big"
	"testing"
	"time"
)

func TestVerifyRecoveryProofBindsEnrolledKeyAndChallenge(t *testing.T) {
	key, certificatePEM := recoveryIdentity(t)
	challenge := RecoveryChallenge{
		ID: "challenge", Nonce: []byte("nonce"),
		PublicKeyFingerprint: "replacement-key",
		CertificatePEM:       certificatePEM + certificatePEM,
	}
	digest := sha256.Sum256(RecoveryMessage(challenge))
	proof, err := ecdsa.SignASN1(rand.Reader, key, digest[:])
	if err != nil {
		t.Fatal(err)
	}
	encoded := base64.RawURLEncoding.EncodeToString(proof)
	if err := VerifyRecoveryProof(challenge, encoded); err != nil {
		t.Fatalf("valid proof rejected: %v", err)
	}

	changed := challenge
	changed.Nonce = []byte("changed")
	if !errors.Is(VerifyRecoveryProof(changed, encoded), ErrInvalidRecoveryProof) {
		t.Fatal("proof was not bound to nonce")
	}
	changed = challenge
	changed.PublicKeyFingerprint = "changed"
	if !errors.Is(VerifyRecoveryProof(changed, encoded), ErrInvalidRecoveryProof) {
		t.Fatal("proof was not bound to replacement key")
	}
	_, unrelatedCertificate := recoveryIdentity(t)
	changed = challenge
	changed.CertificatePEM = unrelatedCertificate
	if !errors.Is(VerifyRecoveryProof(changed, encoded), ErrInvalidRecoveryProof) {
		t.Fatal("proof from an unrelated key was accepted")
	}
}

func recoveryIdentity(t *testing.T) (*ecdsa.PrivateKey, string) {
	t.Helper()
	key, err := ecdsa.GenerateKey(elliptic.P256(), rand.Reader)
	if err != nil {
		t.Fatal(err)
	}
	template := &x509.Certificate{
		SerialNumber: big.NewInt(1), Subject: pkix.Name{CommonName: "device"},
		NotBefore: time.Now().Add(-time.Hour), NotAfter: time.Now().Add(time.Hour),
		KeyUsage: x509.KeyUsageDigitalSignature,
	}
	der, err := x509.CreateCertificate(rand.Reader, template, template, &key.PublicKey, key)
	if err != nil {
		t.Fatal(err)
	}
	return key, string(pem.EncodeToMemory(&pem.Block{Type: "CERTIFICATE", Bytes: der}))
}
