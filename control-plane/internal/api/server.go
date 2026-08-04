package api

import (
	"encoding/json"
	"errors"
	"io"
	"net/http"

	"github.com/agentdesktop-dev/agentdesktop/control-plane/internal/certificate"
	"github.com/agentdesktop-dev/agentdesktop/control-plane/internal/enrollment"
)

type Authenticator interface {
	Authenticate(*http.Request) (enrollment.Principal, error)
}

type Server struct {
	authenticator Authenticator
	enrollments   *enrollment.Service
}

func NewServer(authenticator Authenticator, enrollments *enrollment.Service) http.Handler {
	server := &Server{authenticator: authenticator, enrollments: enrollments}
	mux := http.NewServeMux()
	mux.HandleFunc("POST /v1/enrollments", server.requestEnrollment)
	mux.HandleFunc("GET /healthz", func(response http.ResponseWriter, _ *http.Request) {
		response.WriteHeader(http.StatusOK)
	})
	return mux
}

func (server *Server) requestEnrollment(response http.ResponseWriter, request *http.Request) {
	principal, err := server.authenticator.Authenticate(request)
	if err != nil {
		writeError(response, http.StatusUnauthorized, "invalid_token")
		return
	}
	var body struct {
		CSR string `json:"csr"`
	}
	decoder := json.NewDecoder(http.MaxBytesReader(response, request.Body, 64<<10))
	decoder.DisallowUnknownFields()
	if decoder.Decode(&body) != nil {
		writeError(response, http.StatusBadRequest, "invalid_request")
		return
	}
	if err := decoder.Decode(&struct{}{}); !errors.Is(err, io.EOF) {
		writeError(response, http.StatusBadRequest, "invalid_request")
		return
	}
	record, err := server.enrollments.Request(request.Context(), principal, body.CSR)
	if errors.Is(err, certificate.ErrInvalidCSR) || errors.Is(err, enrollment.ErrInvalidPrincipal) {
		writeError(response, http.StatusBadRequest, "invalid_csr")
		return
	}
	if err != nil {
		writeError(response, http.StatusInternalServerError, "internal_error")
		return
	}
	response.Header().Set("content-type", "application/json")
	response.WriteHeader(http.StatusAccepted)
	_ = json.NewEncoder(response).Encode(record)
}

func writeError(response http.ResponseWriter, status int, code string) {
	response.Header().Set("content-type", "application/json")
	response.WriteHeader(status)
	_ = json.NewEncoder(response).Encode(map[string]any{
		"error": map[string]string{"code": code},
	})
}
