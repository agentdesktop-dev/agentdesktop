package adminui

import (
	"embed"
	"encoding/json"
	"net/http"
)

//go:embed static/*
var assets embed.FS

type Config struct {
	OrganizationName      string `json:"organization_name"`
	AuthorizationEndpoint string `json:"authorization_endpoint"`
	TokenEndpoint         string `json:"token_endpoint"`
	ClientID              string `json:"client_id"`
	Audience              string `json:"audience"`
	Scope                 string `json:"scope"`
}

func Register(mux *http.ServeMux, config Config) {
	mux.HandleFunc("GET /admin", redirect)
	mux.HandleFunc("GET /admin/", serve("static/index.html", "text/html; charset=utf-8"))
	mux.HandleFunc("GET /admin/app.js", serve("static/app.js", "text/javascript; charset=utf-8"))
	mux.HandleFunc("GET /admin/styles.css", serve("static/styles.css", "text/css; charset=utf-8"))
	mux.HandleFunc("GET /v1/admin/ui-config", func(response http.ResponseWriter, _ *http.Request) {
		secure(response)
		response.Header().Set("content-type", "application/json")
		_ = json.NewEncoder(response).Encode(config)
	})
}

func redirect(response http.ResponseWriter, request *http.Request) {
	http.Redirect(response, request, "/admin/", http.StatusTemporaryRedirect)
}

func serve(name, contentType string) http.HandlerFunc {
	contents, err := assets.ReadFile(name)
	if err != nil {
		panic(err)
	}
	return func(response http.ResponseWriter, _ *http.Request) {
		secure(response)
		response.Header().Set("content-type", contentType)
		_, _ = response.Write(contents)
	}
}

func secure(response http.ResponseWriter) {
	response.Header().Set("cache-control", "no-store")
	response.Header().Set("content-security-policy", "default-src 'self'; connect-src 'self' https: http://127.0.0.1:* http://localhost:*; script-src 'self'; style-src 'self'; base-uri 'none'; frame-ancestors 'none'")
	response.Header().Set("referrer-policy", "no-referrer")
	response.Header().Set("x-content-type-options", "nosniff")
}
