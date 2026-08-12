package adminui

import (
	"encoding/json"
	"net/http"
	"net/http/httptest"
	"regexp"
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
	if page.Code != http.StatusOK || !strings.Contains(page.Body.String(), "Agent Desktop Administration") ||
		!strings.Contains(page.Body.String(), `<div id="root"></div>`) {
		t.Fatalf("admin page = %d %q", page.Code, page.Body.String())
	}
	if page.Header().Get("content-security-policy") == "" {
		t.Fatal("admin page has no content security policy")
	}
	script := regexp.MustCompile(`src="\./([^"]+\.js)"`).FindStringSubmatch(page.Body.String())
	if len(script) != 2 {
		t.Fatalf("admin page has no built React module: %q", page.Body.String())
	}
	module := httptest.NewRecorder()
	mux.ServeHTTP(module, httptest.NewRequest(http.MethodGet, "/admin/"+script[1], nil))
	if module.Code != http.StatusOK || !strings.Contains(module.Header().Get("content-type"), "javascript") {
		t.Fatalf("admin module = %d %q", module.Code, module.Header().Get("content-type"))
	}
	legacy := httptest.NewRecorder()
	mux.ServeHTTP(legacy, httptest.NewRequest(http.MethodGet, "/admin/app.js", nil))
	if legacy.Code != http.StatusNotFound {
		t.Fatalf("legacy admin module = %d", legacy.Code)
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
