package certificate

import (
	"crypto/ecdsa"
	"crypto/sha256"
	"crypto/x509"
	"encoding/base64"
	"encoding/pem"
	"errors"
)

var ErrInvalidCSR = errors.New("invalid certificate signing request")

type Request struct {
	DER                  []byte
	PublicKeyFingerprint string
}

func ParseRequest(encoded string) (Request, error) {
	block, rest := pem.Decode([]byte(encoded))
	if block == nil || block.Type != "CERTIFICATE REQUEST" || len(rest) != 0 {
		return Request{}, ErrInvalidCSR
	}
	request, err := x509.ParseCertificateRequest(block.Bytes)
	if err != nil || request.CheckSignature() != nil {
		return Request{}, ErrInvalidCSR
	}
	publicKey, ok := request.PublicKey.(*ecdsa.PublicKey)
	if !ok || publicKey.Curve != nil && publicKey.Curve.Params().Name != "P-256" {
		return Request{}, ErrInvalidCSR
	}
	publicKeyDER, err := x509.MarshalPKIXPublicKey(publicKey)
	if err != nil {
		return Request{}, ErrInvalidCSR
	}
	fingerprint := sha256.Sum256(publicKeyDER)
	return Request{
		DER:                  append([]byte(nil), block.Bytes...),
		PublicKeyFingerprint: base64.RawURLEncoding.EncodeToString(fingerprint[:]),
	}, nil
}
