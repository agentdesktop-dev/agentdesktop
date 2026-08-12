package api

import (
	"encoding/json"
	"errors"
	"io"
	"net/http"
	"strconv"

	"github.com/agentdesktop-dev/agentdesktop/control-plane/internal/adminui"
	"github.com/agentdesktop-dev/agentdesktop/control-plane/internal/agentpolicy"
	"github.com/agentdesktop-dev/agentdesktop/control-plane/internal/certificate"
	"github.com/agentdesktop-dev/agentdesktop/control-plane/internal/deviceidentity"
	"github.com/agentdesktop-dev/agentdesktop/control-plane/internal/discoveryreport"
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
	discoveryReports           *discoveryreport.Service
	discoveryTrustDomain       string
	agentPolicy                *agentpolicy.Service
}

type Option func(*http.ServeMux, *Server)

func WithAdminUI(config adminui.Config) Option {
	return func(mux *http.ServeMux, _ *Server) {
		adminui.Register(mux, config)
	}
}

func WithDiscoveryReports(service *discoveryreport.Service, trustDomain string) Option {
	return func(mux *http.ServeMux, server *Server) {
		server.discoveryReports = service
		server.discoveryTrustDomain = trustDomain
		mux.HandleFunc("PUT /v1/device-reports/current", server.putCurrentDiscoveryReport)
		mux.HandleFunc("GET /v1/device-reports/current/rescan", server.getCurrentDiscoveryRescan)
		mux.HandleFunc("GET /v1/admin/inventory", server.getInventory)
		mux.HandleFunc("GET /v1/admin/inventory/devices", server.getInventoryDevices)
		mux.HandleFunc("POST /v1/admin/discovery-rescans", server.requestDiscoveryRescan)
		mux.HandleFunc("GET /v1/admin/devices/{deviceID}/discovery-report", server.getDeviceDiscoveryReport)
	}
}

func WithAgentPolicy(service *agentpolicy.Service) Option {
	return func(mux *http.ServeMux, server *Server) {
		server.agentPolicy = service
		mux.HandleFunc("GET /v1/admin/agent-policy", server.getAgentPolicy)
		mux.HandleFunc("PUT /v1/admin/agent-policy", server.putAgentPolicy)
	}
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
	mux.HandleFunc("GET /v1/admin/summary", server.getFleetSummary)
	mux.HandleFunc("GET /v1/admin/devices", server.listDevices)
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

func (server *Server) putCurrentDiscoveryReport(response http.ResponseWriter, request *http.Request) {
	device, err := deviceidentity.FromRequest(request, server.discoveryTrustDomain)
	if err != nil {
		writeError(response, http.StatusUnauthorized, "invalid_device_certificate")
		return
	}
	var report discoveryreport.Report
	decoder := json.NewDecoder(http.MaxBytesReader(response, request.Body, discoveryreport.MaxBodyBytes))
	decoder.DisallowUnknownFields()
	if decoder.Decode(&report) != nil {
		writeError(response, http.StatusBadRequest, "invalid_discovery_report")
		return
	}
	if err := decoder.Decode(&struct{}{}); !errors.Is(err, io.EOF) {
		writeError(response, http.StatusBadRequest, "invalid_discovery_report")
		return
	}
	stored, err := server.discoveryReports.PutLatest(request.Context(), device, report)
	switch {
	case errors.Is(err, discoveryreport.ErrInvalidReport):
		writeError(response, http.StatusBadRequest, "invalid_discovery_report")
		return
	case errors.Is(err, discoveryreport.ErrNotActive):
		writeError(response, http.StatusForbidden, "device_not_active")
		return
	case err != nil:
		writeError(response, http.StatusInternalServerError, "internal_error")
		return
	}
	response.Header().Set("content-type", "application/json")
	_ = json.NewEncoder(response).Encode(stored)
}

func (server *Server) getDeviceDiscoveryReport(response http.ResponseWriter, request *http.Request) {
	administrator, err := server.administratorAuthenticator.Authenticate(request)
	if err != nil {
		writeError(response, http.StatusUnauthorized, "invalid_admin_token")
		return
	}
	stored, err := server.discoveryReports.GetLatest(request.Context(), administrator, request.PathValue("deviceID"))
	if errors.Is(err, discoveryreport.ErrNotFound) {
		writeError(response, http.StatusNotFound, "discovery_report_not_found")
		return
	}
	if err != nil {
		writeError(response, http.StatusInternalServerError, "internal_error")
		return
	}
	response.Header().Set("content-type", "application/json")
	_ = json.NewEncoder(response).Encode(stored)
}

func (server *Server) getCurrentDiscoveryRescan(response http.ResponseWriter, request *http.Request) {
	device, err := deviceidentity.FromRequest(request, server.discoveryTrustDomain)
	if err != nil {
		writeError(response, http.StatusUnauthorized, "invalid_device_certificate")
		return
	}
	status, err := server.discoveryReports.RescanStatus(request.Context(), device)
	if errors.Is(err, discoveryreport.ErrNotActive) {
		writeError(response, http.StatusForbidden, "device_not_active")
		return
	}
	if err != nil {
		writeError(response, http.StatusInternalServerError, "internal_error")
		return
	}
	response.Header().Set("content-type", "application/json")
	_ = json.NewEncoder(response).Encode(status)
}

func (server *Server) requestDiscoveryRescan(response http.ResponseWriter, request *http.Request) {
	administrator, err := server.administratorAuthenticator.Authenticate(request)
	if err != nil {
		writeError(response, http.StatusUnauthorized, "invalid_admin_token")
		return
	}
	var body discoveryreport.RescanRequest
	decoder := json.NewDecoder(http.MaxBytesReader(response, request.Body, 64<<10))
	decoder.DisallowUnknownFields()
	if decoder.Decode(&body) != nil {
		writeError(response, http.StatusBadRequest, "invalid_rescan_request")
		return
	}
	if err := decoder.Decode(&struct{}{}); !errors.Is(err, io.EOF) {
		writeError(response, http.StatusBadRequest, "invalid_rescan_request")
		return
	}
	result, err := server.discoveryReports.RequestRescan(request.Context(), administrator, body)
	switch {
	case errors.Is(err, discoveryreport.ErrInvalidRescan):
		writeError(response, http.StatusBadRequest, "invalid_rescan_request")
		return
	case errors.Is(err, discoveryreport.ErrNotActive):
		writeError(response, http.StatusBadRequest, "invalid_rescan_target")
		return
	case err != nil:
		writeError(response, http.StatusInternalServerError, "internal_error")
		return
	}
	response.Header().Set("content-type", "application/json")
	response.WriteHeader(http.StatusAccepted)
	_ = json.NewEncoder(response).Encode(result)
}

func (server *Server) getInventory(response http.ResponseWriter, request *http.Request) {
	administrator, err := server.administratorAuthenticator.Authenticate(request)
	if err != nil {
		writeError(response, http.StatusUnauthorized, "invalid_admin_token")
		return
	}
	limit, offset, err := inventoryPage(request)
	if err != nil {
		writeError(response, http.StatusBadRequest, "invalid_inventory_query")
		return
	}
	page, err := server.discoveryReports.Inventory(request.Context(), administrator, discoveryreport.InventoryQuery{
		Kind: request.URL.Query().Get("kind"), Search: request.URL.Query().Get("q"), Limit: limit, Offset: offset,
	})
	if errors.Is(err, discoveryreport.ErrInvalidReport) {
		writeError(response, http.StatusBadRequest, "invalid_inventory_query")
		return
	}
	if err != nil {
		writeError(response, http.StatusInternalServerError, "internal_error")
		return
	}
	response.Header().Set("content-type", "application/json")
	_ = json.NewEncoder(response).Encode(page)
}

func (server *Server) getInventoryDevices(response http.ResponseWriter, request *http.Request) {
	administrator, err := server.administratorAuthenticator.Authenticate(request)
	if err != nil {
		writeError(response, http.StatusUnauthorized, "invalid_admin_token")
		return
	}
	limit, offset, err := inventoryPage(request)
	if err != nil {
		writeError(response, http.StatusBadRequest, "invalid_inventory_query")
		return
	}
	values := request.URL.Query()
	page, err := server.discoveryReports.InventoryDevices(request.Context(), administrator, discoveryreport.InventoryDeviceQuery{
		Kind: values.Get("kind"), Key: values.Get("key"), Version: values.Get("version"),
		Detail: values.Get("detail"), Search: values.Get("q"), Limit: limit, Offset: offset,
	})
	if errors.Is(err, discoveryreport.ErrInvalidReport) {
		writeError(response, http.StatusBadRequest, "invalid_inventory_query")
		return
	}
	if err != nil {
		writeError(response, http.StatusInternalServerError, "internal_error")
		return
	}
	response.Header().Set("content-type", "application/json")
	_ = json.NewEncoder(response).Encode(page)
}

func inventoryPage(request *http.Request) (int, int, error) {
	limit := 50
	offset := 0
	var err error
	if value := request.URL.Query().Get("limit"); value != "" {
		limit, err = strconv.Atoi(value)
		if err != nil {
			return 0, 0, err
		}
	}
	if value := request.URL.Query().Get("offset"); value != "" {
		offset, err = strconv.Atoi(value)
		if err != nil {
			return 0, 0, err
		}
	}
	return limit, offset, nil
}

func (server *Server) getAgentPolicy(response http.ResponseWriter, request *http.Request) {
	administrator, err := server.administratorAuthenticator.Authenticate(request)
	if err != nil {
		writeError(response, http.StatusUnauthorized, "invalid_admin_token")
		return
	}
	policy, err := server.agentPolicy.Get(request.Context(), administrator)
	if errors.Is(err, agentpolicy.ErrInvalidPolicy) {
		writeError(response, http.StatusBadRequest, "invalid_agent_policy")
		return
	}
	if err != nil {
		writeError(response, http.StatusInternalServerError, "internal_error")
		return
	}
	response.Header().Set("content-type", "application/json")
	_ = json.NewEncoder(response).Encode(policy)
}

func (server *Server) putAgentPolicy(response http.ResponseWriter, request *http.Request) {
	administrator, err := server.administratorAuthenticator.Authenticate(request)
	if err != nil {
		writeError(response, http.StatusUnauthorized, "invalid_admin_token")
		return
	}
	var body agentpolicy.Request
	decoder := json.NewDecoder(http.MaxBytesReader(response, request.Body, 64<<10))
	decoder.DisallowUnknownFields()
	if decoder.Decode(&body) != nil {
		writeError(response, http.StatusBadRequest, "invalid_agent_policy")
		return
	}
	if err := decoder.Decode(&struct{}{}); !errors.Is(err, io.EOF) {
		writeError(response, http.StatusBadRequest, "invalid_agent_policy")
		return
	}
	policy, err := server.agentPolicy.Put(request.Context(), administrator, body)
	switch {
	case errors.Is(err, agentpolicy.ErrInvalidPolicy):
		writeError(response, http.StatusBadRequest, "invalid_agent_policy")
		return
	case err != nil:
		writeError(response, http.StatusInternalServerError, "internal_error")
		return
	}
	response.Header().Set("content-type", "application/json")
	_ = json.NewEncoder(response).Encode(policy)
}

func (server *Server) getFleetSummary(response http.ResponseWriter, request *http.Request) {
	administrator, err := server.administratorAuthenticator.Authenticate(request)
	if err != nil {
		writeError(response, http.StatusUnauthorized, "invalid_admin_token")
		return
	}
	summary, err := server.enrollments.Summary(request.Context(), administrator)
	if errors.Is(err, enrollment.ErrInvalidPrincipal) {
		writeError(response, http.StatusBadRequest, "invalid_request")
		return
	}
	if err != nil {
		writeError(response, http.StatusInternalServerError, "internal_error")
		return
	}
	response.Header().Set("content-type", "application/json")
	_ = json.NewEncoder(response).Encode(summary)
}

func (server *Server) listDevices(response http.ResponseWriter, request *http.Request) {
	administrator, err := server.administratorAuthenticator.Authenticate(request)
	if err != nil {
		writeError(response, http.StatusUnauthorized, "invalid_admin_token")
		return
	}
	devices, err := server.enrollments.ListDevices(request.Context(), administrator, 100)
	if errors.Is(err, enrollment.ErrInvalidPrincipal) {
		writeError(response, http.StatusBadRequest, "invalid_request")
		return
	}
	if err != nil {
		writeError(response, http.StatusInternalServerError, "internal_error")
		return
	}
	response.Header().Set("content-type", "application/json")
	_ = json.NewEncoder(response).Encode(map[string]any{
		"devices": devices,
		"limited": len(devices) == 100,
	})
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
		CSR        string `json:"csr"`
		DeviceName string `json:"device_name"`
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
	record, err := server.enrollments.Request(request.Context(), principal, body.CSR, body.DeviceName)
	if errors.Is(err, certificate.ErrInvalidCSR) {
		writeError(response, http.StatusBadRequest, "invalid_csr")
		return
	}
	if errors.Is(err, enrollment.ErrInvalidPrincipal) || errors.Is(err, enrollment.ErrInvalidDeviceName) {
		writeError(response, http.StatusBadRequest, "invalid_request")
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
