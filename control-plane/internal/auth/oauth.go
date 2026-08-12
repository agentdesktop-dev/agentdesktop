package auth

import (
	"context"
	"crypto"
	"crypto/ecdsa"
	"crypto/elliptic"
	"crypto/rsa"
	"crypto/sha256"
	"encoding/base64"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"math/big"
	"net/http"
	"strings"
	"time"
	"unicode"
	"unicode/utf8"

	"github.com/agentdesktop-dev/agentdesktop/control-plane/internal/enrollment"
)

var ErrInvalidToken = errors.New("invalid OAuth access token")

type Validator struct {
	issuer        string
	audience      string
	requiredScope string
	requiredRole  string
	keys          map[string]verificationKey
	now           func() time.Time
}

type verificationKey struct {
	algorithm string
	key       any
}

type metadata struct {
	Issuer  string `json:"issuer"`
	JWKSURI string `json:"jwks_uri"`
}

type keySet struct {
	Keys []jsonWebKey `json:"keys"`
}

type jsonWebKey struct {
	Algorithm string `json:"alg"`
	Curve     string `json:"crv"`
	Exponent  string `json:"e"`
	KeyID     string `json:"kid"`
	KeyType   string `json:"kty"`
	Modulus   string `json:"n"`
	Use       string `json:"use"`
	X         string `json:"x"`
	Y         string `json:"y"`
}

type tokenHeader struct {
	Algorithm string `json:"alg"`
	KeyID     string `json:"kid"`
	Type      string `json:"typ"`
}

type tokenClaims struct {
	Audience          audience `json:"aud"`
	Expires           int64    `json:"exp"`
	IssuedAt          int64    `json:"iat"`
	Issuer            string   `json:"iss"`
	PreferredUsername string   `json:"preferred_username"`
	Scope             string   `json:"scope"`
	Subject           string   `json:"sub"`
	RealmAccess       struct {
		Roles []string `json:"roles"`
	} `json:"realm_access"`
}

func (validator *Validator) RequireRealmRole(role string) *Validator {
	validator.requiredRole = role
	return validator
}

type audience []string

func (value *audience) UnmarshalJSON(encoded []byte) error {
	var single string
	if json.Unmarshal(encoded, &single) == nil {
		*value = []string{single}
		return nil
	}
	var multiple []string
	if err := json.Unmarshal(encoded, &multiple); err != nil {
		return err
	}
	*value = multiple
	return nil
}

func Discover(ctx context.Context, client *http.Client, issuer, expectedAudience, requiredScope string) (*Validator, error) {
	if client == nil || issuer == "" || expectedAudience == "" || requiredScope == "" {
		return nil, errors.New("OAuth validator configuration is incomplete")
	}
	discoveryURL := strings.TrimRight(issuer, "/") + "/.well-known/oauth-authorization-server"
	var discovered metadata
	if err := getJSON(ctx, client, discoveryURL, &discovered); err != nil {
		return nil, fmt.Errorf("discover OAuth issuer: %w", err)
	}
	if discovered.Issuer != issuer || discovered.JWKSURI == "" {
		return nil, errors.New("OAuth discovery returned mismatched issuer metadata")
	}
	var document keySet
	if err := getJSON(ctx, client, discovered.JWKSURI, &document); err != nil {
		return nil, fmt.Errorf("load OAuth JWKS: %w", err)
	}
	keys := make(map[string]verificationKey)
	for _, encoded := range document.Keys {
		key, err := parseKey(encoded)
		if err == nil && encoded.KeyID != "" {
			keys[encoded.KeyID] = key
		}
	}
	if len(keys) == 0 {
		return nil, errors.New("OAuth JWKS contains no supported signing key")
	}
	return &Validator{
		issuer: issuer, audience: expectedAudience, requiredScope: requiredScope,
		keys: keys, now: time.Now,
	}, nil
}

func (validator *Validator) Authenticate(request *http.Request) (enrollment.Principal, error) {
	authorization := request.Header.Get("authorization")
	token, ok := strings.CutPrefix(authorization, "Bearer ")
	if !ok || token == "" {
		return enrollment.Principal{}, ErrInvalidToken
	}
	segments := strings.Split(token, ".")
	if len(segments) != 3 {
		return enrollment.Principal{}, ErrInvalidToken
	}
	var header tokenHeader
	if decodeJSONSegment(segments[0], &header) != nil || header.KeyID == "" {
		return enrollment.Principal{}, ErrInvalidToken
	}
	key, ok := validator.keys[header.KeyID]
	if !ok || key.algorithm != header.Algorithm || !verifySignature(key, segments) {
		return enrollment.Principal{}, ErrInvalidToken
	}
	var claims tokenClaims
	if decodeJSONSegment(segments[1], &claims) != nil {
		return enrollment.Principal{}, ErrInvalidToken
	}
	now := validator.now().Unix()
	if claims.Issuer != validator.issuer || claims.Subject == "" ||
		claims.Expires <= now || claims.IssuedAt > now+60 ||
		!contains(claims.Audience, validator.audience) ||
		!contains(strings.Fields(claims.Scope), validator.requiredScope) ||
		(validator.requiredRole != "" && !contains(claims.RealmAccess.Roles, validator.requiredRole)) {
		return enrollment.Principal{}, ErrInvalidToken
	}
	return enrollment.Principal{
		Issuer:      claims.Issuer,
		Subject:     claims.Subject,
		DisplayName: displayName(claims.PreferredUsername),
	}, nil
}

func displayName(value string) string {
	value = strings.TrimSpace(value)
	if value == "" || utf8.RuneCountInString(value) > 256 || strings.IndexFunc(value, unicode.IsControl) >= 0 {
		return ""
	}
	return value
}

func getJSON(ctx context.Context, client *http.Client, target string, destination any) error {
	request, err := http.NewRequestWithContext(ctx, http.MethodGet, target, nil)
	if err != nil {
		return err
	}
	response, err := client.Do(request)
	if err != nil {
		return err
	}
	defer response.Body.Close()
	if response.StatusCode != http.StatusOK {
		return fmt.Errorf("unexpected status %d", response.StatusCode)
	}
	return json.NewDecoder(io.LimitReader(response.Body, 1<<20)).Decode(destination)
}

func parseKey(encoded jsonWebKey) (verificationKey, error) {
	if encoded.Use != "" && encoded.Use != "sig" {
		return verificationKey{}, ErrInvalidToken
	}
	switch encoded.Algorithm {
	case "ES256":
		if encoded.KeyType != "EC" || encoded.Curve != "P-256" {
			return verificationKey{}, ErrInvalidToken
		}
		x, err := decodeInteger(encoded.X)
		if err != nil {
			return verificationKey{}, err
		}
		y, err := decodeInteger(encoded.Y)
		if err != nil || !elliptic.P256().IsOnCurve(x, y) {
			return verificationKey{}, ErrInvalidToken
		}
		return verificationKey{algorithm: encoded.Algorithm, key: &ecdsa.PublicKey{Curve: elliptic.P256(), X: x, Y: y}}, nil
	case "RS256":
		if encoded.KeyType != "RSA" {
			return verificationKey{}, ErrInvalidToken
		}
		modulus, err := decodeInteger(encoded.Modulus)
		if err != nil {
			return verificationKey{}, err
		}
		exponentBytes, err := base64.RawURLEncoding.DecodeString(encoded.Exponent)
		if err != nil || len(exponentBytes) == 0 || len(exponentBytes) > 4 {
			return verificationKey{}, ErrInvalidToken
		}
		exponent := 0
		for _, value := range exponentBytes {
			exponent = exponent<<8 | int(value)
		}
		if modulus.BitLen() < 2048 || exponent < 3 || exponent%2 == 0 {
			return verificationKey{}, ErrInvalidToken
		}
		return verificationKey{algorithm: encoded.Algorithm, key: &rsa.PublicKey{N: modulus, E: exponent}}, nil
	default:
		return verificationKey{}, ErrInvalidToken
	}
}

func verifySignature(key verificationKey, segments []string) bool {
	signature, err := base64.RawURLEncoding.DecodeString(segments[2])
	if err != nil {
		return false
	}
	digest := sha256.Sum256([]byte(segments[0] + "." + segments[1]))
	switch publicKey := key.key.(type) {
	case *ecdsa.PublicKey:
		if len(signature) != 64 {
			return false
		}
		return ecdsa.Verify(publicKey, digest[:], new(big.Int).SetBytes(signature[:32]), new(big.Int).SetBytes(signature[32:]))
	case *rsa.PublicKey:
		return rsa.VerifyPKCS1v15(publicKey, crypto.SHA256, digest[:], signature) == nil
	default:
		return false
	}
}

func decodeInteger(encoded string) (*big.Int, error) {
	value, err := base64.RawURLEncoding.DecodeString(encoded)
	if err != nil || len(value) == 0 {
		return nil, ErrInvalidToken
	}
	return new(big.Int).SetBytes(value), nil
}

func decodeJSONSegment(segment string, destination any) error {
	decoded, err := base64.RawURLEncoding.DecodeString(segment)
	if err != nil {
		return err
	}
	return json.Unmarshal(decoded, destination)
}

func contains(values []string, expected string) bool {
	for _, value := range values {
		if value == expected {
			return true
		}
	}
	return false
}
