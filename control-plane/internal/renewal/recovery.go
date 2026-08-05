package renewal

import (
	"crypto/ecdsa"
	"crypto/sha256"
	"crypto/x509"
	"encoding/base64"
	"encoding/pem"
	"errors"
)

var ErrInvalidRecoveryProof = errors.New("invalid recovery proof")

func RecoveryMessage(challenge RecoveryChallenge) []byte {
	return []byte("agentdesktop-device-recovery-v1\n" + challenge.ID + "\n" +
		base64.RawURLEncoding.EncodeToString(challenge.Nonce) + "\n" +
		challenge.PublicKeyFingerprint)
}

func VerifyRecoveryProof(challenge RecoveryChallenge, encodedProof string) error {
	block, _ := pem.Decode([]byte(challenge.CertificatePEM))
	if block == nil || block.Type != "CERTIFICATE" {
		return ErrInvalidRecoveryProof
	}
	certificate, err := x509.ParseCertificate(block.Bytes)
	if err != nil {
		return ErrInvalidRecoveryProof
	}
	publicKey, ok := certificate.PublicKey.(*ecdsa.PublicKey)
	if !ok || publicKey.Curve.Params().Name != "P-256" {
		return ErrInvalidRecoveryProof
	}
	proof, err := base64.RawURLEncoding.DecodeString(encodedProof)
	digest := sha256.Sum256(RecoveryMessage(challenge))
	if err != nil || !ecdsa.VerifyASN1(publicKey, digest[:], proof) {
		return ErrInvalidRecoveryProof
	}
	return nil
}
