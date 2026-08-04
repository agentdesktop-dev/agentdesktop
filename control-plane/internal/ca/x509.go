package ca

import (
	"context"
	"crypto"
	"crypto/ecdsa"
	"crypto/rand"
	"crypto/sha256"
	"crypto/x509"
	"encoding/hex"
	"encoding/pem"
	"errors"
	"fmt"
	"math/big"
	"net/url"
	"os"
	"path/filepath"
	"strings"
	"time"
)

const defaultBackdate = 5 * time.Minute

type X509Issuer struct {
	certificate *x509.Certificate
	chainPEM    string
	lifetime    time.Duration
	signer      crypto.Signer
	trustDomain string
}

func NewX509Issuer(
	certificate *x509.Certificate,
	chainPEM string,
	signer crypto.Signer,
	trustDomain string,
	lifetime time.Duration,
) (*X509Issuer, error) {
	if certificate == nil || signer == nil || !certificate.IsCA ||
		certificate.KeyUsage&x509.KeyUsageCertSign == 0 {
		return nil, errors.New("issuer certificate is not a certificate authority")
	}
	if lifetime <= 0 || lifetime > 7*24*time.Hour {
		return nil, errors.New("client certificate lifetime must be between zero and seven days")
	}
	identity, err := spiffeURI(trustDomain, "00000000-0000-0000-0000-000000000000", "00000000-0000-0000-0000-000000000000")
	if err != nil || identity.Host != trustDomain {
		return nil, errors.New("invalid SPIFFE trust domain")
	}
	if !publicKeysEqual(certificate.PublicKey, signer.Public()) {
		return nil, errors.New("issuer certificate and private key do not match")
	}
	return &X509Issuer{
		certificate: certificate,
		chainPEM:    chainPEM,
		lifetime:    lifetime,
		signer:      signer,
		trustDomain: trustDomain,
	}, nil
}

func LoadX509Issuer(certificatePath, keyPath, trustDomain string, lifetime time.Duration) (*X509Issuer, error) {
	certificatePEM, err := os.ReadFile(filepath.Clean(certificatePath))
	if err != nil {
		return nil, fmt.Errorf("read issuer certificate: %w", err)
	}
	block, rest := pem.Decode(certificatePEM)
	if block == nil || block.Type != "CERTIFICATE" || len(strings.TrimSpace(string(rest))) != 0 {
		return nil, errors.New("issuer certificate file must contain one PEM certificate")
	}
	certificate, err := x509.ParseCertificate(block.Bytes)
	if err != nil {
		return nil, fmt.Errorf("parse issuer certificate: %w", err)
	}
	keyInfo, err := os.Lstat(filepath.Clean(keyPath))
	if err != nil {
		return nil, fmt.Errorf("inspect issuer key: %w", err)
	}
	if !keyInfo.Mode().IsRegular() || keyInfo.Mode().Perm()&0o077 != 0 {
		return nil, errors.New("issuer key must be a regular owner-only file")
	}
	keyPEM, err := os.ReadFile(filepath.Clean(keyPath))
	if err != nil {
		return nil, fmt.Errorf("read issuer key: %w", err)
	}
	keyBlock, keyRest := pem.Decode(keyPEM)
	if keyBlock == nil || len(strings.TrimSpace(string(keyRest))) != 0 {
		return nil, errors.New("issuer key file must contain one PEM private key")
	}
	signer, err := parseSigner(keyBlock.Bytes)
	if err != nil {
		return nil, err
	}
	return NewX509Issuer(certificate, string(certificatePEM), signer, trustDomain, lifetime)
}

func (issuer *X509Issuer) Issue(ctx context.Context, request IssuanceRequest) (Certificate, error) {
	if err := ctx.Err(); err != nil {
		return Certificate{}, err
	}
	if request.ID == "" || request.IssuedAt.IsZero() {
		return Certificate{}, errors.New("certificate request identity and issuance time are required")
	}
	csr, err := x509.ParseCertificateRequest(request.CSRDER)
	if err != nil || csr.CheckSignature() != nil {
		return Certificate{}, errors.New("invalid certificate signing request")
	}
	publicKey, ok := csr.PublicKey.(*ecdsa.PublicKey)
	if !ok || publicKey.Curve.Params().Name != "P-256" {
		return Certificate{}, errors.New("client certificate key must use P-256")
	}
	identityURI, err := spiffeURI(issuer.trustDomain, request.Identity.OrganizationID, request.Identity.DeviceID)
	if err != nil {
		return Certificate{}, err
	}
	serialDigest := sha256.Sum256([]byte("agentdesktop-client-certificate:" + request.ID))
	serialBytes := serialDigest[:20]
	serialBytes[0] &= 0x7f
	serial := new(big.Int).SetBytes(serialBytes)
	if serial.Sign() == 0 {
		serial.SetInt64(1)
	}
	issuedAt := request.IssuedAt.UTC()
	notBefore := issuedAt.Add(-defaultBackdate)
	if notBefore.Before(issuer.certificate.NotBefore) {
		notBefore = issuer.certificate.NotBefore
	}
	notAfter := issuedAt.Add(issuer.lifetime)
	if notAfter.After(issuer.certificate.NotAfter) {
		notAfter = issuer.certificate.NotAfter
	}
	if !notAfter.After(issuedAt) {
		return Certificate{}, errors.New("issuer certificate expires before client certificate")
	}
	template := &x509.Certificate{
		SerialNumber:          serial,
		NotBefore:             notBefore,
		NotAfter:              notAfter,
		KeyUsage:              x509.KeyUsageDigitalSignature,
		ExtKeyUsage:           []x509.ExtKeyUsage{x509.ExtKeyUsageClientAuth},
		URIs:                  []*url.URL{identityURI},
		BasicConstraintsValid: true,
	}
	der, err := x509.CreateCertificate(rand.Reader, template, issuer.certificate, publicKey, issuer.signer)
	if err != nil {
		return Certificate{}, err
	}
	leafPEM := pem.EncodeToMemory(&pem.Block{Type: "CERTIFICATE", Bytes: der})
	return Certificate{
		ChainPEM:     string(leafPEM) + issuer.chainPEM,
		NotBefore:    notBefore,
		NotAfter:     notAfter,
		SerialNumber: strings.ToLower(hex.EncodeToString(serial.Bytes())),
	}, nil
}

func spiffeURI(trustDomain, organizationID, deviceID string) (*url.URL, error) {
	if trustDomain == "" || strings.ContainsAny(trustDomain, "/?#@:") ||
		organizationID == "" || strings.Contains(organizationID, "/") ||
		deviceID == "" || strings.Contains(deviceID, "/") {
		return nil, errors.New("invalid certificate identity")
	}
	return url.Parse("spiffe://" + trustDomain + "/organization/" + url.PathEscape(organizationID) + "/device/" + url.PathEscape(deviceID))
}

func parseSigner(der []byte) (crypto.Signer, error) {
	if key, err := x509.ParsePKCS8PrivateKey(der); err == nil {
		if signer, ok := key.(crypto.Signer); ok {
			return signer, nil
		}
	}
	if key, err := x509.ParseECPrivateKey(der); err == nil {
		return key, nil
	}
	return nil, errors.New("issuer key is not a supported private key")
}

func publicKeysEqual(left, right any) bool {
	leftDER, leftErr := x509.MarshalPKIXPublicKey(left)
	rightDER, rightErr := x509.MarshalPKIXPublicKey(right)
	return leftErr == nil && rightErr == nil && string(leftDER) == string(rightDER)
}
