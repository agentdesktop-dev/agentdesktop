package deviceidentity

import (
	"errors"
	"net/http"
	"net/url"
	"strings"
)

type Identity struct {
	OrganizationID string
	DeviceID       string
	SerialNumber   string
}

func FromRequest(request *http.Request, trustDomain string) (Identity, error) {
	if request == nil || request.TLS == nil || len(request.TLS.VerifiedChains) != 1 ||
		len(request.TLS.VerifiedChains[0]) == 0 {
		return Identity{}, errors.New("verified client certificate is required")
	}
	leaf := request.TLS.VerifiedChains[0][0]
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

func validUUID(value string) bool {
	if len(value) != 36 {
		return false
	}
	for index, character := range value {
		if index == 8 || index == 13 || index == 18 || index == 23 {
			if character != '-' {
				return false
			}
			continue
		}
		if !((character >= '0' && character <= '9') || (character >= 'a' && character <= 'f') ||
			(character >= 'A' && character <= 'F')) {
			return false
		}
	}
	return true
}
