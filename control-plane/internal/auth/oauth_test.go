package auth

import (
	"context"
	"crypto"
	"crypto/ecdsa"
	"crypto/elliptic"
	"crypto/rand"
	"crypto/rsa"
	"crypto/sha256"
	"encoding/base64"
	"encoding/json"
	"math/big"
	"net/http"
	"net/http/httptest"
	"testing"
	"time"
)

func TestValidatorAuthenticatesIssuerSignedBearerToken(t *testing.T) {
	key, err := ecdsa.GenerateKey(elliptic.P256(), rand.Reader)
	if err != nil {
		t.Fatal(err)
	}
	var issuer string
	server := httptest.NewServer(http.HandlerFunc(func(response http.ResponseWriter, request *http.Request) {
		switch request.URL.Path {
		case "/.well-known/oauth-authorization-server":
			encodeJSON(t, response, metadata{Issuer: issuer, JWKSURI: issuer + "/jwks"})
		case "/jwks":
			encodeJSON(t, response, keySet{Keys: []jsonWebKey{{
				Algorithm: "ES256", Curve: "P-256", KeyID: "test-key", KeyType: "EC", Use: "sig",
				X: encodeInteger(key.X), Y: encodeInteger(key.Y),
			}}})
		default:
			http.NotFound(response, request)
		}
	}))
	defer server.Close()
	issuer = server.URL

	validator, err := Discover(context.Background(), server.Client(), issuer, "agentdesktop", "agentgateway.invoke")
	if err != nil {
		t.Fatal(err)
	}
	validator.now = func() time.Time { return time.Unix(1_000, 0) }
	token := signToken(t, key, map[string]any{
		"iss": issuer, "sub": "user-1", "aud": "agentdesktop",
		"scope": "openid agentgateway.invoke", "iat": 900, "exp": 1100,
	})
	request := httptest.NewRequest(http.MethodPost, "/v1/enrollments", nil)
	request.Header.Set("authorization", "Bearer "+token)

	principal, err := validator.Authenticate(request)
	if err != nil {
		t.Fatal(err)
	}
	if principal.Issuer != issuer || principal.Subject != "user-1" {
		t.Fatalf("principal = %#v", principal)
	}

	tampered := request.Clone(request.Context())
	tampered.Header.Set("authorization", "Bearer "+token+"x")
	if _, err := validator.Authenticate(tampered); err != ErrInvalidToken {
		t.Fatalf("tampered token error = %v", err)
	}
}

func TestValidatorAuthenticatesRS256BearerToken(t *testing.T) {
	key, err := rsa.GenerateKey(rand.Reader, 2048)
	if err != nil {
		t.Fatal(err)
	}
	encodedKey := jsonWebKey{
		Algorithm: "RS256", KeyID: "rsa-key", KeyType: "RSA", Use: "sig",
		Modulus: encodeInteger(key.N), Exponent: base64.RawURLEncoding.EncodeToString(big.NewInt(int64(key.E)).Bytes()),
	}
	parsedKey, err := parseKey(encodedKey)
	if err != nil {
		t.Fatal(err)
	}
	validator := &Validator{
		issuer: "https://issuer.example/", audience: "agentdesktop", requiredScope: "agentgateway.invoke",
		keys: map[string]verificationKey{"rsa-key": parsedKey}, now: func() time.Time { return time.Unix(1_000, 0) },
	}
	header, _ := json.Marshal(map[string]string{"alg": "RS256", "kid": "rsa-key", "typ": "at+jwt"})
	payload, _ := json.Marshal(map[string]any{
		"iss": validator.issuer, "sub": "user-1", "aud": validator.audience,
		"scope": validator.requiredScope, "iat": 900, "exp": 1100,
	})
	input := base64.RawURLEncoding.EncodeToString(header) + "." + base64.RawURLEncoding.EncodeToString(payload)
	digest := sha256.Sum256([]byte(input))
	signature, err := rsa.SignPKCS1v15(rand.Reader, key, crypto.SHA256, digest[:])
	if err != nil {
		t.Fatal(err)
	}
	request := httptest.NewRequest(http.MethodPost, "/v1/enrollments", nil)
	request.Header.Set("authorization", "Bearer "+input+"."+base64.RawURLEncoding.EncodeToString(signature))
	if _, err := validator.Authenticate(request); err != nil {
		t.Fatal(err)
	}
}

func signToken(t *testing.T, key *ecdsa.PrivateKey, claims map[string]any) string {
	t.Helper()
	header, _ := json.Marshal(map[string]string{"alg": "ES256", "kid": "test-key", "typ": "at+jwt"})
	payload, _ := json.Marshal(claims)
	input := base64.RawURLEncoding.EncodeToString(header) + "." + base64.RawURLEncoding.EncodeToString(payload)
	digest := sha256.Sum256([]byte(input))
	r, s, err := ecdsa.Sign(rand.Reader, key, digest[:])
	if err != nil {
		t.Fatal(err)
	}
	signature := make([]byte, 64)
	r.FillBytes(signature[:32])
	s.FillBytes(signature[32:])
	return input + "." + base64.RawURLEncoding.EncodeToString(signature)
}

func encodeInteger(value *big.Int) string {
	return base64.RawURLEncoding.EncodeToString(value.Bytes())
}

func encodeJSON(t *testing.T, response http.ResponseWriter, value any) {
	t.Helper()
	response.Header().Set("content-type", "application/json")
	if err := json.NewEncoder(response).Encode(value); err != nil {
		t.Fatal(err)
	}
}
