package adminui

import (
	"embed"
	"encoding/json"
	"io/fs"
	"net/http"
)

//go:embed static
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
	staticAssets, err := fs.Sub(assets, "static")
	if err != nil {
		panic(err)
	}
	staticHandler := http.StripPrefix("/admin/", http.FileServer(http.FS(staticAssets)))
	mux.HandleFunc("GET /admin", redirect)
	mux.Handle("GET /admin/", secureHandler(staticHandler))
	mux.HandleFunc("GET /v1/admin/ui-config", func(response http.ResponseWriter, _ *http.Request) {
		secure(response)
		response.Header().Set("content-type", "application/json")
		_ = json.NewEncoder(response).Encode(config)
	})
}

func redirect(response http.ResponseWriter, request *http.Request) {
	http.Redirect(response, request, "/admin/", http.StatusTemporaryRedirect)
}

func secureHandler(next http.Handler) http.Handler {
	return http.HandlerFunc(func(response http.ResponseWriter, request *http.Request) {
		secure(response)
		next.ServeHTTP(response, request)
	})
}

func secure(response http.ResponseWriter) {
	response.Header().Set("cache-control", "no-store")
	response.Header().Set("content-security-policy", "default-src 'self'; connect-src 'self' https: http://127.0.0.1:* http://localhost:*; font-src 'self' data:; img-src 'self' data:; script-src 'self'; style-src 'self'; base-uri 'none'; frame-ancestors 'none'")
	response.Header().Set("referrer-policy", "no-referrer")
	response.Header().Set("x-content-type-options", "nosniff")
}
