package api

import (
	"crypto/x509"
	"encoding/json"
	"errors"
	"io"
	"net/http"
	"net/url"

	"github.com/agentdesktop-dev/agentdesktop/control-plane/internal/certificate"
	"github.com/agentdesktop-dev/agentdesktop/control-plane/internal/deviceauthorization"
	"github.com/agentdesktop-dev/agentdesktop/control-plane/internal/deviceidentity"
	"github.com/agentdesktop-dev/agentdesktop/control-plane/internal/enrollment"
	"github.com/agentdesktop-dev/agentdesktop/control-plane/internal/renewal"
)

type Authenticator interface {
	Authenticate(*http.Request) (enrollment.Principal, error)
}

type Server struct {
	administratorAuthenticator Authenticator
	authenticator              Authenticator
	enrollments                *enrollment.Service
}

type Option func(*http.ServeMux, *Server)

func WithDeviceAuthorization(
	authorizations *deviceauthorization.Service,
	deviceTrustDomain string,
	deviceRoots *x509.CertPool,
	gatewayIdentity *url.URL,
) Option {
	return func(mux *http.ServeMux, server *Server) {
		mux.HandleFunc("POST /v1/gateway/device-authorizations", func(response http.ResponseWriter, request *http.Request) {
			server.authorizeDevice(
				response, request, authorizations, deviceTrustDomain, deviceRoots, gatewayIdentity,
			)
		})
	}
}

func (server *Server) authorizeDevice(
	response http.ResponseWriter,
	request *http.Request,
	authorizations *deviceauthorization.Service,
	deviceTrustDomain string,
	deviceRoots *x509.CertPool,
	gatewayIdentity *url.URL,
) {
	if !hasVerifiedServiceIdentity(request, gatewayIdentity) {
		writeError(response, http.StatusUnauthorized, "invalid_gateway_identity")
		return
	}
	var body struct {
		CertificatePEM string `json:"certificate_pem"`
		Issuer         string `json:"issuer"`
		Subject        string `json:"subject"`
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
	device, err := deviceidentity.FromPEM(body.CertificatePEM, deviceTrustDomain, deviceRoots)
	if err != nil {
		writeError(response, http.StatusForbidden, "device_not_authorized")
		return
	}
	err = authorizations.Authorize(
		request.Context(), enrollment.Principal{Issuer: body.Issuer, Subject: body.Subject}, device,
	)
	switch {
	case errors.Is(err, deviceauthorization.ErrInvalidRequest):
		writeError(response, http.StatusBadRequest, "invalid_request")
		return
	case errors.Is(err, deviceauthorization.ErrDenied):
		writeError(response, http.StatusForbidden, "device_not_authorized")
		return
	case err != nil:
		writeError(response, http.StatusInternalServerError, "internal_error")
		return
	}
	response.Header().Set("content-type", "application/json")
	_ = json.NewEncoder(response).Encode(map[string]bool{"authorized": true})
}

func hasVerifiedServiceIdentity(request *http.Request, expected *url.URL) bool {
	if request == nil || expected == nil || request.TLS == nil || len(request.TLS.VerifiedChains) != 1 ||
		len(request.TLS.VerifiedChains[0]) == 0 {
		return false
	}
	leaf := request.TLS.VerifiedChains[0][0]
	return len(leaf.URIs) == 1 && leaf.URIs[0].String() == expected.String()
}

func WithRenewal(authenticator Authenticator, renewals *renewal.Service, trustDomain string) Option {
	return func(mux *http.ServeMux, server *Server) {
		mux.HandleFunc("POST /v1/renewals", func(response http.ResponseWriter, request *http.Request) {
			server.renewCertificate(response, request, authenticator, renewals, trustDomain)
		})
		mux.HandleFunc("POST /v1/recovery/challenges", func(response http.ResponseWriter, request *http.Request) {
			server.createRecoveryChallenge(response, request, authenticator, renewals)
		})
		mux.HandleFunc("POST /v1/recovery", func(response http.ResponseWriter, request *http.Request) {
			server.recoverCertificate(response, request, authenticator, renewals)
		})
	}
}

func (server *Server) createRecoveryChallenge(
	response http.ResponseWriter,
	request *http.Request,
	authenticator Authenticator,
	renewals *renewal.Service,
) {
	principal, err := authenticator.Authenticate(request)
	if err != nil {
		writeError(response, http.StatusUnauthorized, "invalid_token")
		return
	}
	var body struct {
		DeviceID              string `json:"device_id"`
		PresentedSerialNumber string `json:"presented_serial_number"`
		CSR                   string `json:"csr"`
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
	challenge, err := renewals.CreateRecoveryChallenge(
		request.Context(), principal, body.DeviceID, body.PresentedSerialNumber, body.CSR,
	)
	switch {
	case errors.Is(err, certificate.ErrInvalidCSR), errors.Is(err, renewal.ErrInvalidRequest):
		writeError(response, http.StatusBadRequest, "invalid_csr")
		return
	case errors.Is(err, renewal.ErrNotActive):
		writeError(response, http.StatusForbidden, "device_not_active")
		return
	case err != nil:
		writeError(response, http.StatusInternalServerError, "internal_error")
		return
	}
	response.Header().Set("content-type", "application/json")
	response.WriteHeader(http.StatusCreated)
	_ = json.NewEncoder(response).Encode(challenge)
}

func (server *Server) recoverCertificate(
	response http.ResponseWriter,
	request *http.Request,
	authenticator Authenticator,
	renewals *renewal.Service,
) {
	principal, err := authenticator.Authenticate(request)
	if err != nil {
		writeError(response, http.StatusUnauthorized, "invalid_token")
		return
	}
	var body struct {
		ChallengeID string `json:"challenge_id"`
		Proof       string `json:"proof"`
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
	recovered, err := renewals.Recover(request.Context(), principal, body.ChallengeID, body.Proof)
	switch {
	case errors.Is(err, renewal.ErrInvalidRequest):
		writeError(response, http.StatusBadRequest, "invalid_request")
		return
	case errors.Is(err, renewal.ErrInvalidRecoveryProof):
		writeError(response, http.StatusUnauthorized, "invalid_recovery_proof")
		return
	case errors.Is(err, renewal.ErrNotActive):
		writeError(response, http.StatusForbidden, "device_not_active")
		return
	case errors.Is(err, renewal.ErrIssuanceFailed):
		writeError(response, http.StatusBadGateway, "certificate_issuance_failed")
		return
	case err != nil:
		writeError(response, http.StatusInternalServerError, "internal_error")
		return
	}
	response.Header().Set("content-type", "application/json")
	_ = json.NewEncoder(response).Encode(recovered)
}

func NewServer(
	authenticator Authenticator,
	administratorAuthenticator Authenticator,
	enrollments *enrollment.Service,
	options ...Option,
) http.Handler {
	server := &Server{
		authenticator:              authenticator,
		administratorAuthenticator: administratorAuthenticator,
		enrollments:                enrollments,
	}
	mux := http.NewServeMux()
	mux.HandleFunc("POST /v1/enrollments", server.requestEnrollment)
	mux.HandleFunc("GET /v1/enrollments/{enrollmentID}", server.getEnrollment)
	mux.HandleFunc("GET /v1/admin/enrollments", server.listEnrollments)
	mux.HandleFunc("POST /v1/admin/enrollments/{enrollmentID}/approve", server.approveEnrollment)
	mux.HandleFunc("POST /v1/admin/enrollments/{enrollmentID}/reject", server.rejectEnrollment)
	mux.HandleFunc("POST /v1/admin/devices/{deviceID}/revoke", server.revokeDevice)
	mux.HandleFunc("GET /healthz", func(response http.ResponseWriter, _ *http.Request) {
		response.WriteHeader(http.StatusOK)
	})
	for _, option := range options {
		option(mux, server)
	}
	return mux
}

func (server *Server) renewCertificate(
	response http.ResponseWriter,
	request *http.Request,
	authenticator Authenticator,
	renewals *renewal.Service,
	trustDomain string,
) {
	principal, err := authenticator.Authenticate(request)
	if err != nil {
		writeError(response, http.StatusUnauthorized, "invalid_token")
		return
	}
	device, err := deviceidentity.FromRequest(request, trustDomain)
	if err != nil {
		writeError(response, http.StatusUnauthorized, "invalid_device_certificate")
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
	renewed, err := renewals.Renew(request.Context(), principal, device, body.CSR)
	switch {
	case errors.Is(err, certificate.ErrInvalidCSR), errors.Is(err, renewal.ErrInvalidRequest):
		writeError(response, http.StatusBadRequest, "invalid_csr")
		return
	case errors.Is(err, renewal.ErrNotActive):
		writeError(response, http.StatusForbidden, "device_not_active")
		return
	case errors.Is(err, renewal.ErrIssuanceFailed):
		writeError(response, http.StatusBadGateway, "certificate_issuance_failed")
		return
	case err != nil:
		writeError(response, http.StatusInternalServerError, "internal_error")
		return
	}
	response.Header().Set("content-type", "application/json")
	_ = json.NewEncoder(response).Encode(renewed)
}

func (server *Server) revokeDevice(response http.ResponseWriter, request *http.Request) {
	administrator, err := server.administratorAuthenticator.Authenticate(request)
	if err != nil {
		writeError(response, http.StatusUnauthorized, "invalid_admin_token")
		return
	}
	revocation, err := server.enrollments.RevokeDevice(
		request.Context(),
		administrator,
		request.PathValue("deviceID"),
	)
	switch {
	case errors.Is(err, enrollment.ErrNotActive):
		writeError(response, http.StatusConflict, "device_not_active")
		return
	case errors.Is(err, enrollment.ErrInvalidPrincipal):
		writeError(response, http.StatusBadRequest, "invalid_request")
		return
	case err != nil:
		writeError(response, http.StatusInternalServerError, "internal_error")
		return
	}
	response.Header().Set("content-type", "application/json")
	_ = json.NewEncoder(response).Encode(revocation)
}

func (server *Server) listEnrollments(response http.ResponseWriter, request *http.Request) {
	administrator, err := server.administratorAuthenticator.Authenticate(request)
	if err != nil {
		writeError(response, http.StatusUnauthorized, "invalid_admin_token")
		return
	}
	status := request.URL.Query().Get("status")
	if status == "" {
		status = "pending"
	}
	records, err := server.enrollments.List(request.Context(), administrator, status, 100)
	if errors.Is(err, enrollment.ErrInvalidPrincipal) {
		writeError(response, http.StatusBadRequest, "invalid_request")
		return
	}
	if err != nil {
		writeError(response, http.StatusInternalServerError, "internal_error")
		return
	}
	response.Header().Set("content-type", "application/json")
	_ = json.NewEncoder(response).Encode(map[string]any{"enrollments": records})
}

func (server *Server) rejectEnrollment(response http.ResponseWriter, request *http.Request) {
	administrator, err := server.administratorAuthenticator.Authenticate(request)
	if err != nil {
		writeError(response, http.StatusUnauthorized, "invalid_admin_token")
		return
	}
	record, err := server.enrollments.Reject(
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
	case err != nil:
		writeError(response, http.StatusInternalServerError, "internal_error")
		return
	}
	response.Header().Set("content-type", "application/json")
	_ = json.NewEncoder(response).Encode(record)
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
