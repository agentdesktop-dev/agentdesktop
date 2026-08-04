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
	administratorAuthenticator Authenticator
	authenticator              Authenticator
	enrollments                *enrollment.Service
}

func NewServer(
	authenticator Authenticator,
	administratorAuthenticator Authenticator,
	enrollments *enrollment.Service,
) http.Handler {
	server := &Server{
		authenticator:              authenticator,
		administratorAuthenticator: administratorAuthenticator,
		enrollments:                enrollments,
	}
	mux := http.NewServeMux()
	mux.HandleFunc("POST /v1/enrollments", server.requestEnrollment)
	mux.HandleFunc("GET /v1/enrollments/{enrollmentID}", server.getEnrollment)
	mux.HandleFunc("POST /v1/admin/enrollments/{enrollmentID}/approve", server.approveEnrollment)
	mux.HandleFunc("GET /healthz", func(response http.ResponseWriter, _ *http.Request) {
		response.WriteHeader(http.StatusOK)
	})
	return mux
}

func (server *Server) getEnrollment(response http.ResponseWriter, request *http.Request) {
	principal, err := server.authenticator.Authenticate(request)
	if err != nil {
		writeError(response, http.StatusUnauthorized, "invalid_token")
		return
	}
	record, err := server.enrollments.Get(request.Context(), principal, request.PathValue("enrollmentID"))
	switch {
	case errors.Is(err, enrollment.ErrNotFound):
		writeError(response, http.StatusNotFound, "enrollment_not_found")
		return
	case errors.Is(err, enrollment.ErrInvalidPrincipal):
		writeError(response, http.StatusBadRequest, "invalid_request")
		return
	case err != nil:
		writeError(response, http.StatusInternalServerError, "internal_error")
		return
	}
	response.Header().Set("content-type", "application/json")
	_ = json.NewEncoder(response).Encode(record)
}

func (server *Server) approveEnrollment(response http.ResponseWriter, request *http.Request) {
	administrator, err := server.administratorAuthenticator.Authenticate(request)
	if err != nil {
		writeError(response, http.StatusUnauthorized, "invalid_admin_token")
		return
	}
	approval, err := server.enrollments.Approve(
		request.Context(),
		administrator,
		request.PathValue("enrollmentID"),
	)
	switch {
	case errors.Is(err, enrollment.ErrNotPending):
		writeError(response, http.StatusConflict, "enrollment_not_pending")
		return
	case errors.Is(err, enrollment.ErrInvalidPrincipal):
		writeError(response, http.StatusBadRequest, "invalid_request")
		return
	case errors.Is(err, enrollment.ErrIssuanceFailed):
		writeError(response, http.StatusBadGateway, "certificate_issuance_failed")
		return
	case err != nil:
		writeError(response, http.StatusInternalServerError, "internal_error")
		return
	}
	response.Header().Set("content-type", "application/json")
	response.WriteHeader(http.StatusOK)
	_ = json.NewEncoder(response).Encode(approval)
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
