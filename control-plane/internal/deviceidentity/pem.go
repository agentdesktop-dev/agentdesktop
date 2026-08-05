package deviceidentity

import (
	"crypto/x509"
	"encoding/pem"
	"errors"
	"net/url"
	"strings"
)

func FromPEM(encoded string, trustDomain string, roots *x509.CertPool) (Identity, error) {
	block, _ := pem.Decode([]byte(encoded))
	if block == nil || block.Type != "CERTIFICATE" || roots == nil {
		return Identity{}, errors.New("submitted device certificate is invalid")
	}
	leaf, err := x509.ParseCertificate(block.Bytes)
	if err != nil {
		return Identity{}, errors.New("submitted device certificate is invalid")
	}
	if _, err := leaf.Verify(x509.VerifyOptions{
		Roots: roots, KeyUsages: []x509.ExtKeyUsage{x509.ExtKeyUsageClientAuth},
	}); err != nil {
		return Identity{}, errors.New("device certificate is not trusted")
	}
	if len(leaf.URIs) != 1 || leaf.SerialNumber == nil || leaf.SerialNumber.Sign() <= 0 {
		return Identity{}, errors.New("client certificate identity is missing or ambiguous")
	}
	identityURI := leaf.URIs[0]
	if identityURI.Scheme != "spiffe" || identityURI.Host != trustDomain ||
		identityURI.User != nil || identityURI.RawQuery != "" || identityURI.Fragment != "" {
		return Identity{}, errors.New("client certificate identity is outside the configured trust domain")
	}
	segments := strings.Split(strings.TrimPrefix(identityURI.EscapedPath(), "/"), "/")
	if len(segments) != 4 || segments[0] != "organization" || segments[2] != "device" {
		return Identity{}, errors.New("client certificate identity path is invalid")
	}
	organizationID, organizationErr := url.PathUnescape(segments[1])
	deviceID, deviceErr := url.PathUnescape(segments[3])
	if organizationErr != nil || deviceErr != nil || !validUUID(organizationID) || !validUUID(deviceID) {
		return Identity{}, errors.New("client certificate identity contains an invalid identifier")
	}
	return Identity{
		OrganizationID: organizationID,
		DeviceID:       deviceID,
		SerialNumber:   strings.ToLower(leaf.SerialNumber.Text(16)),
	}, nil
}
