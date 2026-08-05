package adminui

import (
	"encoding/json"
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"
)

func TestRegisterServesUIAndPublicOAuthConfiguration(t *testing.T) {
	mux := http.NewServeMux()
	Register(mux, Config{
		OrganizationName:      "Acme",
		AuthorizationEndpoint: "https://login.example/authorize",
		TokenEndpoint:         "https://login.example/token",
		ClientID:              "agentdesktop-admin",
		Audience:              "agentdesktop",
		Scope:                 "agentdesktop.enrollment.admin",
	})

	page := httptest.NewRecorder()
	mux.ServeHTTP(page, httptest.NewRequest(http.MethodGet, "/admin/", nil))
	if page.Code != http.StatusOK || !strings.Contains(page.Body.String(), "Device administration") {
		t.Fatalf("admin page = %d %q", page.Code, page.Body.String())
	}
	if page.Header().Get("content-security-policy") == "" {
		t.Fatal("admin page has no content security policy")
	}

	configuration := httptest.NewRecorder()
	mux.ServeHTTP(configuration, httptest.NewRequest(http.MethodGet, "/v1/admin/ui-config", nil))
	var decoded Config
	if err := json.NewDecoder(configuration.Body).Decode(&decoded); err != nil {
		t.Fatal(err)
	}
	if decoded.OrganizationName != "Acme" || decoded.Scope != "agentdesktop.enrollment.admin" {
		t.Fatalf("config = %#v", decoded)
	}
}
