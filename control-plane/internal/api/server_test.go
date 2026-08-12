package api

import (
	"bytes"
	"context"
	"crypto/ecdsa"
	"crypto/elliptic"
	"crypto/rand"
	"crypto/tls"
	"crypto/x509"
	"encoding/json"
	"encoding/pem"
	"errors"
	"math/big"
	"net/http"
	"net/http/httptest"
	"net/url"
	"testing"
	"time"

	"github.com/agentdesktop-dev/agentdesktop/control-plane/internal/agentpolicy"
	"github.com/agentdesktop-dev/agentdesktop/control-plane/internal/ca"
	"github.com/agentdesktop-dev/agentdesktop/control-plane/internal/certificate"
	"github.com/agentdesktop-dev/agentdesktop/control-plane/internal/deviceidentity"
	"github.com/agentdesktop-dev/agentdesktop/control-plane/internal/discoveryreport"
	"github.com/agentdesktop-dev/agentdesktop/control-plane/internal/enrollment"
	"github.com/agentdesktop-dev/agentdesktop/control-plane/internal/renewal"
)

type testAuthenticator struct {
	principal enrollment.Principal
	err       error
}

type recordingDiscoveryStore struct {
	device         deviceidentity.Identity
	report         discoveryreport.Report
	stored         discoveryreport.StoredReport
	inventory      discoveryreport.InventoryPage
	inventoryQuery discoveryreport.InventoryQuery
	devices        discoveryreport.InventoryDevicePage
	deviceQuery    discoveryreport.InventoryDeviceQuery
	rescanRequest  discoveryreport.RescanRequest
	rescanResult   discoveryreport.RescanResult
	rescanStatus   discoveryreport.RescanStatus
}

type recordingAgentPolicyStore struct {
	request agentpolicy.Request
	policy  agentpolicy.Policy
	err     error
}

func (store *recordingAgentPolicyStore) PutAgentPolicy(_ context.Context, _ enrollment.Principal, request agentpolicy.Request) (agentpolicy.Policy, error) {
	store.request = request
	return store.policy, store.err
}

func (store *recordingAgentPolicyStore) GetAgentPolicy(context.Context, enrollment.Principal) (agentpolicy.Policy, error) {
	return store.policy, store.err
}

func (store *recordingDiscoveryStore) PutLatestDiscoveryReport(_ context.Context, device deviceidentity.Identity, _ int, encoded []byte) (discoveryreport.StoredReport, error) {
	store.device = device
	if err := json.Unmarshal(encoded, &store.report); err != nil {
		return discoveryreport.StoredReport{}, err
	}
	return store.stored, nil
}

func (store *recordingDiscoveryStore) GetLatestDiscoveryReport(context.Context, enrollment.Principal, string) (discoveryreport.StoredReport, error) {
	return store.stored, nil
}

func (store *recordingDiscoveryStore) ListInventoryAssets(_ context.Context, _ enrollment.Principal, query discoveryreport.InventoryQuery) (discoveryreport.InventoryPage, error) {
	store.inventoryQuery = query
	return store.inventory, nil
}

func (store *recordingDiscoveryStore) ListInventoryDevices(_ context.Context, _ enrollment.Principal, query discoveryreport.InventoryDeviceQuery) (discoveryreport.InventoryDevicePage, error) {
	store.deviceQuery = query
	return store.devices, nil
}

func (store *recordingDiscoveryStore) RequestDiscoveryRescan(_ context.Context, _ enrollment.Principal, request discoveryreport.RescanRequest) (discoveryreport.RescanResult, error) {
	store.rescanRequest = request
	return store.rescanResult, nil
}

func (store *recordingDiscoveryStore) GetDiscoveryRescanStatus(context.Context, deviceidentity.Identity) (discoveryreport.RescanStatus, error) {
	return store.rescanStatus, nil
}

func (authenticator testAuthenticator) Authenticate(*http.Request) (enrollment.Principal, error) {
	return authenticator.principal, authenticator.err
}

type recordingStore struct {
	principal    enrollment.Principal
	deviceName   string
	issuance     enrollment.Issuance
	status       enrollment.Status
	getErr       error
	adminRecords []enrollment.AdministrativeRecord
	adminDevices []enrollment.AdministrativeDevice
	adminSummary enrollment.FleetSummary
}

func (store *recordingStore) Get(
	_ context.Context,
	principal enrollment.Principal,
	_ string,
) (enrollment.Status, error) {
	store.principal = principal
	return store.status, store.getErr
}

func (store *recordingStore) ListIssuing(context.Context, time.Time, int) ([]enrollment.Issuance, error) {
	return nil, nil
}

func (store *recordingStore) List(context.Context, enrollment.Principal, string, int) ([]enrollment.AdministrativeRecord, error) {
	return store.adminRecords, nil
}

func (store *recordingStore) ListDevices(context.Context, enrollment.Principal, int) ([]enrollment.AdministrativeDevice, error) {
	return store.adminDevices, nil
}

func (store *recordingStore) Summary(context.Context, enrollment.Principal) (enrollment.FleetSummary, error) {
	return store.adminSummary, nil
}

func (store *recordingStore) Reject(_ context.Context, _ enrollment.Principal, id string) (enrollment.AdministrativeRecord, error) {
	return enrollment.AdministrativeRecord{EnrollmentID: id, Status: "rejected"}, nil
}

func (store *recordingStore) RevokeDevice(_ context.Context, _ enrollment.Principal, id string) (enrollment.DeviceRevocation, error) {
	return enrollment.DeviceRevocation{DeviceID: id, Status: "revoked"}, nil
}

func (store *recordingStore) CreatePending(
	_ context.Context,
	principal enrollment.Principal,
	request certificate.Request,
	deviceName string,
	id string,
) (enrollment.Enrollment, error) {
	store.principal = principal
	store.deviceName = deviceName
	return enrollment.Enrollment{
		ID: id, Status: "pending", Issuer: principal.Issuer, Subject: principal.Subject,
		DeviceName: deviceName, PublicKeyFingerprint: request.PublicKeyFingerprint,
	}, nil
}

func (store *recordingStore) BeginIssuance(
	_ context.Context,
	_ enrollment.Principal,
	enrollmentID string,
	deviceID string,
) (enrollment.Issuance, error) {
	store.issuance.EnrollmentID = enrollmentID
	store.issuance.DeviceID = deviceID
	return store.issuance, nil
}

func (store *recordingStore) CompleteIssuance(
	_ context.Context,
	_ enrollment.Principal,
	issuance enrollment.Issuance,
	certificate enrollment.IssuedCertificate,
) (enrollment.Approval, error) {
	return enrollment.Approval{
		EnrollmentID:   issuance.EnrollmentID,
		Status:         "approved",
		DeviceID:       issuance.DeviceID,
		CertificatePEM: certificate.ChainPEM,
		SerialNumber:   certificate.SerialNumber,
		NotBefore:      certificate.NotBefore,
		NotAfter:       certificate.NotAfter,
	}, nil
}

type apiIssuer struct{}

func (apiIssuer) Issue(context.Context, ca.IssuanceRequest) (ca.Certificate, error) {
	return ca.Certificate{
		ChainPEM:     "certificate-chain",
		SerialNumber: "01",
		NotBefore:    time.Unix(1_000, 0),
		NotAfter:     time.Unix(2_000, 0),
	}, nil
}

type renewalStore struct {
	principal enrollment.Principal
	device    deviceidentity.Identity
	request   certificate.Request
}

func (store *renewalStore) Begin(
	_ context.Context,
	principal enrollment.Principal,
	device deviceidentity.Identity,
	request certificate.Request,
	_ string,
) (renewal.Claim, error) {
	store.principal = principal
	store.device = device
	store.request = request
	return renewal.Claim{
		ID:                   "33333333-3333-4333-8333-333333333333",
		OrganizationID:       device.OrganizationID,
		DeviceID:             device.DeviceID,
		CSRDER:               request.DER,
		PublicKeyFingerprint: request.PublicKeyFingerprint,
		StartedAt:            time.Unix(1_000, 0),
	}, nil
}

func (store *renewalStore) CreateRecoveryChallenge(
	context.Context,
	enrollment.Principal,
	string,
	string,
	certificate.Request,
	string,
	[]byte,
	time.Time,
) (renewal.RecoveryChallenge, error) {
	return renewal.RecoveryChallenge{}, errors.New("unexpected recovery challenge")
}

func (store *renewalStore) GetRecoveryChallenge(
	context.Context,
	enrollment.Principal,
	string,
) (renewal.RecoveryChallenge, error) {
	return renewal.RecoveryChallenge{}, errors.New("unexpected recovery challenge")
}

func (store *renewalStore) BeginRecovery(
	context.Context,
	enrollment.Principal,
	renewal.RecoveryChallenge,
	string,
) (renewal.Claim, error) {
	return renewal.Claim{}, errors.New("unexpected recovery")
}

func (store *renewalStore) Complete(
	_ context.Context,
	_ enrollment.Principal,
	claim renewal.Claim,
	issued renewal.Certificate,
) (renewal.Response, error) {
	return renewal.Response{
		RenewalID: claim.ID, Status: "approved", DeviceID: claim.DeviceID,
		PublicKeyFingerprint: claim.PublicKeyFingerprint, Certificate: issued,
	}, nil
}

func (store *renewalStore) ListIssuingRenewals(context.Context, time.Time, int) ([]renewal.Claim, error) {
	return nil, nil
}

func TestEnrollmentUsesAuthenticatedPrincipal(t *testing.T) {
	store := &recordingStore{}
	handler := NewServer(
		testAuthenticator{principal: enrollment.Principal{Issuer: "https://issuer.example/", Subject: "user-1"}},
		testAuthenticator{principal: enrollment.Principal{Issuer: "https://issuer.example/", Subject: "admin-1"}},
		enrollment.NewService(store, apiIssuer{}),
	)
	body, err := json.Marshal(map[string]string{"csr": signedCSR(t), "device_name": "  workstation-7  "})
	if err != nil {
		t.Fatal(err)
	}
	request := httptest.NewRequest(http.MethodPost, "/v1/enrollments", bytes.NewReader(body))
	response := httptest.NewRecorder()
	handler.ServeHTTP(response, request)

	if response.Code != http.StatusAccepted {
		t.Fatalf("status = %d, body = %s", response.Code, response.Body.String())
	}
	if store.principal.Subject != "user-1" || store.principal.Issuer != "https://issuer.example/" {
		t.Fatalf("stored principal = %#v", store.principal)
	}
	if store.deviceName != "workstation-7" {
		t.Fatalf("stored device name = %q", store.deviceName)
	}
}

func TestRenewalRequiresOAuthAndVerifiedDeviceCertificate(t *testing.T) {
	const (
		organizationID = "11111111-1111-4111-8111-111111111111"
		userID         = "33333333-3333-4333-8333-333333333333"
		deviceID       = "22222222-2222-4222-8222-222222222222"
		trustDomain    = "devices.example.com"
	)
	owner := enrollment.Principal{Issuer: "https://issuer.example/", Subject: "user-1"}
	store := &renewalStore{}
	handler := NewServer(
		testAuthenticator{principal: owner},
		testAuthenticator{},
		enrollment.NewService(&recordingStore{}, apiIssuer{}),
		WithRenewal(testAuthenticator{principal: owner}, renewal.NewService(store, apiIssuer{}), trustDomain),
	)
	body, err := json.Marshal(map[string]string{"csr": signedCSR(t)})
	if err != nil {
		t.Fatal(err)
	}
	request := httptest.NewRequest(http.MethodPost, "/v1/renewals", bytes.NewReader(body))
	response := httptest.NewRecorder()
	handler.ServeHTTP(response, request)
	if response.Code != http.StatusUnauthorized {
		t.Fatalf("renewal without device certificate status = %d", response.Code)
	}

	identityURI, err := url.Parse("spiffe://" + trustDomain + "/ns/" + organizationID + "/sa/user." + userID + ".device." + deviceID)
	if err != nil {
		t.Fatal(err)
	}
	leaf := &x509.Certificate{SerialNumber: big.NewInt(1), URIs: []*url.URL{identityURI}}
	request = httptest.NewRequest(http.MethodPost, "/v1/renewals", bytes.NewReader(body))
	request.TLS = &tls.ConnectionState{
		PeerCertificates: []*x509.Certificate{leaf},
		VerifiedChains:   [][]*x509.Certificate{{leaf}},
	}
	response = httptest.NewRecorder()
	handler.ServeHTTP(response, request)
	if response.Code != http.StatusOK {
		t.Fatalf("renewal status = %d, body = %s", response.Code, response.Body.String())
	}
	if store.principal != owner || store.device.OrganizationID != organizationID ||
		store.device.DeviceID != deviceID || store.device.SerialNumber != "1" ||
		store.request.PublicKeyFingerprint == "" {
		t.Fatalf("renewal identity = %#v, principal = %#v", store.device, store.principal)
	}
}

func TestDiscoveryReportRequiresVerifiedDeviceCertificateAndSupportsAdminRead(t *testing.T) {
	const (
		organizationID = "11111111-1111-4111-8111-111111111111"
		userID         = "33333333-3333-4333-8333-333333333333"
		deviceID       = "22222222-2222-4222-8222-222222222222"
		trustDomain    = "devices.example.com"
	)
	report := discoveryreport.Report{
		SchemaVersion: discoveryreport.SchemaVersion, CollectorVersion: "0.1.0", Platform: "macos",
		Coverage: discoveryreport.Coverage{ProjectScopes: "not_scanned"},
		Agents:   []discoveryreport.Agent{{ID: "claude-code", Installed: true, Running: "detected", Evidence: []string{"executable"}}},
	}
	encoded, err := json.Marshal(report)
	if err != nil {
		t.Fatal(err)
	}
	store := &recordingDiscoveryStore{stored: discoveryreport.StoredReport{DeviceID: deviceID, Report: report}}
	handler := NewServer(
		testAuthenticator{},
		testAuthenticator{principal: enrollment.Principal{Issuer: "https://issuer.example/", Subject: "admin-1"}},
		enrollment.NewService(&recordingStore{}, apiIssuer{}),
		WithDiscoveryReports(discoveryreport.NewService(store), trustDomain),
	)
	request := httptest.NewRequest(http.MethodPut, "/v1/device-reports/current", bytes.NewReader(encoded))
	response := httptest.NewRecorder()
	handler.ServeHTTP(response, request)
	if response.Code != http.StatusUnauthorized {
		t.Fatalf("report without certificate status = %d", response.Code)
	}
	identityURI, _ := url.Parse("spiffe://" + trustDomain + "/ns/" + organizationID + "/sa/user." + userID + ".device." + deviceID)
	leaf := &x509.Certificate{SerialNumber: big.NewInt(1), URIs: []*url.URL{identityURI}}
	request = httptest.NewRequest(http.MethodPut, "/v1/device-reports/current", bytes.NewReader(encoded))
	request.TLS = &tls.ConnectionState{PeerCertificates: []*x509.Certificate{leaf}, VerifiedChains: [][]*x509.Certificate{{leaf}}}
	response = httptest.NewRecorder()
	handler.ServeHTTP(response, request)
	if response.Code != http.StatusOK || store.device.DeviceID != deviceID || store.report.Agents[0].ID != "claude-code" {
		t.Fatalf("report status = %d, body = %s, device = %#v", response.Code, response.Body.String(), store.device)
	}
	injected := append(encoded[:len(encoded)-1], []byte(`,"device_id":"`+deviceID+`"}`)...)
	request = httptest.NewRequest(http.MethodPut, "/v1/device-reports/current", bytes.NewReader(injected))
	request.TLS = &tls.ConnectionState{PeerCertificates: []*x509.Certificate{leaf}, VerifiedChains: [][]*x509.Certificate{{leaf}}}
	response = httptest.NewRecorder()
	handler.ServeHTTP(response, request)
	if response.Code != http.StatusBadRequest {
		t.Fatalf("identity-bearing report status = %d, body = %s", response.Code, response.Body.String())
	}
	response = httptest.NewRecorder()
	handler.ServeHTTP(response, httptest.NewRequest(http.MethodGet, "/v1/admin/devices/"+deviceID+"/discovery-report", nil))
	if response.Code != http.StatusOK || !bytes.Contains(response.Body.Bytes(), []byte(`"device_id":"`+deviceID+`"`)) {
		t.Fatalf("admin discovery status = %d, body = %s", response.Code, response.Body.String())
	}
}

func TestAdministratorInventorySupportsPagedAssetsAndDeviceDrillDown(t *testing.T) {
	administrator := enrollment.Principal{Issuer: "https://issuer.example/", Subject: "admin-1"}
	store := &recordingDiscoveryStore{
		inventory: discoveryreport.InventoryPage{Kind: "agent", Total: 4},
		devices:   discoveryreport.InventoryDevicePage{Total: 17},
	}
	handler := NewServer(
		testAuthenticator{}, testAuthenticator{principal: administrator},
		enrollment.NewService(&recordingStore{}, apiIssuer{}),
		WithDiscoveryReports(discoveryreport.NewService(store), "devices.example.com"),
	)
	response := httptest.NewRecorder()
	handler.ServeHTTP(response, httptest.NewRequest(http.MethodGet, "/v1/admin/inventory?kind=agent&q=claude&limit=25&offset=50", nil))
	if response.Code != http.StatusOK || store.inventoryQuery != (discoveryreport.InventoryQuery{Kind: "agent", Search: "claude", Limit: 25, Offset: 50}) {
		t.Fatalf("inventory status = %d, query = %#v, body = %s", response.Code, store.inventoryQuery, response.Body.String())
	}
	response = httptest.NewRecorder()
	handler.ServeHTTP(response, httptest.NewRequest(http.MethodGet, "/v1/admin/inventory/devices?kind=mcp&key=github&detail=stdio&q=macbook&limit=20", nil))
	if response.Code != http.StatusOK || store.deviceQuery.Kind != "mcp" || store.deviceQuery.Key != "github" ||
		store.deviceQuery.Detail != "stdio" || store.deviceQuery.Search != "macbook" || store.deviceQuery.Limit != 20 {
		t.Fatalf("inventory devices status = %d, query = %#v, body = %s", response.Code, store.deviceQuery, response.Body.String())
	}
	response = httptest.NewRecorder()
	handler.ServeHTTP(response, httptest.NewRequest(http.MethodGet, "/v1/admin/inventory?kind=unknown", nil))
	if response.Code != http.StatusBadRequest {
		t.Fatalf("invalid inventory status = %d, body = %s", response.Code, response.Body.String())
	}
}

func TestAdministratorRequestsRescanAndDevicePollsWithCertificate(t *testing.T) {
	const (
		organizationID = "11111111-1111-4111-8111-111111111111"
		userID         = "33333333-3333-4333-8333-333333333333"
		deviceID       = "22222222-2222-4222-8222-222222222222"
		trustDomain    = "devices.example.com"
	)
	store := &recordingDiscoveryStore{
		rescanResult: discoveryreport.RescanResult{Requested: 1},
		rescanStatus: discoveryreport.RescanStatus{Pending: true},
	}
	handler := NewServer(
		testAuthenticator{}, testAuthenticator{principal: enrollment.Principal{Issuer: "issuer", Subject: "admin"}},
		enrollment.NewService(&recordingStore{}, apiIssuer{}),
		WithDiscoveryReports(discoveryreport.NewService(store), trustDomain),
	)
	response := httptest.NewRecorder()
	handler.ServeHTTP(response, httptest.NewRequest(http.MethodPost, "/v1/admin/discovery-rescans", bytes.NewBufferString(`{"target_mode":"selected","device_ids":["`+deviceID+`"]}`)))
	if response.Code != http.StatusAccepted || store.rescanRequest.TargetMode != "selected" || store.rescanRequest.DeviceIDs[0] != deviceID {
		t.Fatalf("rescan request status = %d, request = %#v, body = %s", response.Code, store.rescanRequest, response.Body.String())
	}
	response = httptest.NewRecorder()
	handler.ServeHTTP(response, httptest.NewRequest(http.MethodGet, "/v1/device-reports/current/rescan", nil))
	if response.Code != http.StatusUnauthorized {
		t.Fatalf("rescan poll without certificate status = %d", response.Code)
	}
	identityURI, _ := url.Parse("spiffe://" + trustDomain + "/ns/" + organizationID + "/sa/user." + userID + ".device." + deviceID)
	leaf := &x509.Certificate{SerialNumber: big.NewInt(1), URIs: []*url.URL{identityURI}}
	request := httptest.NewRequest(http.MethodGet, "/v1/device-reports/current/rescan", nil)
	request.TLS = &tls.ConnectionState{PeerCertificates: []*x509.Certificate{leaf}, VerifiedChains: [][]*x509.Certificate{{leaf}}}
	response = httptest.NewRecorder()
	handler.ServeHTTP(response, request)
	if response.Code != http.StatusOK || !bytes.Contains(response.Body.Bytes(), []byte(`"pending":true`)) {
		t.Fatalf("rescan poll status = %d, body = %s", response.Code, response.Body.String())
	}
}

func TestAdministratorCanGetAndUpdateAgentPolicy(t *testing.T) {
	administrator := enrollment.Principal{Issuer: "https://issuer.example/", Subject: "admin-1"}
	rules := agentpolicy.Default().Rules
	rules[2].Action = "deny"
	store := &recordingAgentPolicyStore{policy: agentpolicy.Policy{
		SchemaVersion: agentpolicy.SchemaVersion, Rules: rules, Configured: true, Enforcement: "not_available",
	}}
	handler := NewServer(
		testAuthenticator{}, testAuthenticator{principal: administrator},
		enrollment.NewService(&recordingStore{}, apiIssuer{}),
		WithAgentPolicy(agentpolicy.NewService(store)),
	)
	response := httptest.NewRecorder()
	handler.ServeHTTP(response, httptest.NewRequest(http.MethodGet, "/v1/admin/agent-policy", nil))
	if response.Code != http.StatusOK || !bytes.Contains(response.Body.Bytes(), []byte(`"agent_id":"codex-cli","action":"deny"`)) {
		t.Fatalf("get policy status = %d, body = %s", response.Code, response.Body.String())
	}
	body, err := json.Marshal(agentpolicy.Request{SchemaVersion: agentpolicy.SchemaVersion, Rules: rules})
	if err != nil {
		t.Fatal(err)
	}
	response = httptest.NewRecorder()
	handler.ServeHTTP(response, httptest.NewRequest(http.MethodPut, "/v1/admin/agent-policy", bytes.NewReader(body)))
	if response.Code != http.StatusOK || len(store.request.Rules) != 5 || store.request.Rules[2].Action != "deny" {
		t.Fatalf("put policy status = %d, request = %#v, body = %s", response.Code, store.request, response.Body.String())
	}
	response = httptest.NewRecorder()
	handler.ServeHTTP(response, httptest.NewRequest(http.MethodPut, "/v1/admin/agent-policy", bytes.NewBufferString(`{"schema_version":1,"rules":[]}`)))
	if response.Code != http.StatusBadRequest {
		t.Fatalf("invalid policy status = %d, body = %s", response.Code, response.Body.String())
	}
}

func TestEnrollmentRejectsUnauthenticatedAndUnknownFields(t *testing.T) {
	service := enrollment.NewService(&recordingStore{}, apiIssuer{})
	unauthenticated := NewServer(testAuthenticator{err: errors.New("invalid token")}, testAuthenticator{}, service)
	response := httptest.NewRecorder()
	unauthenticated.ServeHTTP(response, httptest.NewRequest(http.MethodPost, "/v1/enrollments", bytes.NewBufferString(`{}`)))
	if response.Code != http.StatusUnauthorized {
		t.Fatalf("unauthenticated status = %d", response.Code)
	}

	authenticated := NewServer(testAuthenticator{principal: enrollment.Principal{Issuer: "issuer", Subject: "subject"}}, testAuthenticator{}, service)
	response = httptest.NewRecorder()
	authenticated.ServeHTTP(response, httptest.NewRequest(http.MethodPost, "/v1/enrollments", bytes.NewBufferString(`{"csr":"value","subject":"attacker"}`)))
	if response.Code != http.StatusBadRequest {
		t.Fatalf("unknown identity field status = %d", response.Code)
	}
}

func TestApprovalRequiresAdministratorAndReturnsCertificate(t *testing.T) {
	store := &recordingStore{issuance: enrollment.Issuance{
		OrganizationID: "organization-1", CSRDER: []byte("csr"), StartedAt: time.Unix(1_000, 0),
	}}
	administrator := testAuthenticator{principal: enrollment.Principal{Issuer: "https://issuer.example/", Subject: "admin-1"}}
	handler := NewServer(
		testAuthenticator{},
		administrator,
		enrollment.NewService(store, apiIssuer{}),
	)
	response := httptest.NewRecorder()
	handler.ServeHTTP(response, httptest.NewRequest(http.MethodPost, "/v1/admin/enrollments/enrollment-1/approve", nil))
	if response.Code != http.StatusOK {
		t.Fatalf("status = %d, body = %s", response.Code, response.Body.String())
	}
	var approval enrollment.Approval
	if err := json.Unmarshal(response.Body.Bytes(), &approval); err != nil {
		t.Fatal(err)
	}
	if approval.EnrollmentID != "enrollment-1" || approval.CertificatePEM != "certificate-chain" {
		t.Fatalf("approval = %#v", approval)
	}

	unauthorized := NewServer(testAuthenticator{}, testAuthenticator{err: errors.New("not admin")}, enrollment.NewService(store, apiIssuer{}))
	response = httptest.NewRecorder()
	unauthorized.ServeHTTP(response, httptest.NewRequest(http.MethodPost, "/v1/admin/enrollments/enrollment-1/approve", nil))
	if response.Code != http.StatusUnauthorized {
		t.Fatalf("unauthorized status = %d", response.Code)
	}
}

func TestEnrollmentStatusUsesAuthenticatedOwnerAndReturnsCertificate(t *testing.T) {
	store := &recordingStore{status: enrollment.Status{
		EnrollmentID: "enrollment-1",
		Status:       "approved",
		Certificate: &enrollment.IssuedCertificate{
			ChainPEM: "certificate-chain",
		},
	}}
	owner := enrollment.Principal{Issuer: "https://issuer.example/", Subject: "user-1"}
	handler := NewServer(testAuthenticator{principal: owner}, testAuthenticator{}, enrollment.NewService(store, apiIssuer{}))
	response := httptest.NewRecorder()
	handler.ServeHTTP(response, httptest.NewRequest(http.MethodGet, "/v1/enrollments/enrollment-1", nil))
	if response.Code != http.StatusOK {
		t.Fatalf("status = %d, body = %s", response.Code, response.Body.String())
	}
	var record enrollment.Status
	if err := json.Unmarshal(response.Body.Bytes(), &record); err != nil {
		t.Fatal(err)
	}
	if store.principal != owner || record.Certificate == nil || record.Certificate.ChainPEM != "certificate-chain" {
		t.Fatalf("record = %#v, principal = %#v", record, store.principal)
	}

	store.getErr = enrollment.ErrNotFound
	response = httptest.NewRecorder()
	handler.ServeHTTP(response, httptest.NewRequest(http.MethodGet, "/v1/enrollments/foreign", nil))
	if response.Code != http.StatusNotFound {
		t.Fatalf("foreign enrollment status = %d", response.Code)
	}
}

func TestAdministratorListsAndRejectsPendingEnrollment(t *testing.T) {
	store := &recordingStore{adminRecords: []enrollment.AdministrativeRecord{{
		EnrollmentID: "enrollment-1",
		Status:       "pending",
		Subject:      "user-1",
		Username:     "employee",
		DeviceName:   "workstation-7",
	}}, adminDevices: []enrollment.AdministrativeDevice{{
		DeviceID: "device-1",
		Status:   "active",
		Subject:  "user-1",
		Username: "employee",
	}}, adminSummary: enrollment.FleetSummary{ActiveDevices: 1, PendingEnrollments: 1}}
	administrator := testAuthenticator{principal: enrollment.Principal{
		Issuer: "https://issuer.example/", Subject: "admin-1",
	}}
	handler := NewServer(testAuthenticator{}, administrator, enrollment.NewService(store, apiIssuer{}))

	response := httptest.NewRecorder()
	handler.ServeHTTP(response, httptest.NewRequest(http.MethodGet, "/v1/admin/summary", nil))
	if response.Code != http.StatusOK || !bytes.Contains(response.Body.Bytes(), []byte(`"active_devices":1`)) {
		t.Fatalf("summary status = %d, body = %s", response.Code, response.Body.String())
	}

	response = httptest.NewRecorder()
	handler.ServeHTTP(response, httptest.NewRequest(http.MethodGet, "/v1/admin/devices", nil))
	if response.Code != http.StatusOK ||
		!bytes.Contains(response.Body.Bytes(), []byte(`"device_id":"device-1"`)) ||
		!bytes.Contains(response.Body.Bytes(), []byte(`"username":"employee"`)) {
		t.Fatalf("device list status = %d, body = %s", response.Code, response.Body.String())
	}

	response = httptest.NewRecorder()
	handler.ServeHTTP(response, httptest.NewRequest(http.MethodGet, "/v1/admin/enrollments", nil))
	if response.Code != http.StatusOK ||
		!bytes.Contains(response.Body.Bytes(), []byte(`"enrollment_id":"enrollment-1"`)) ||
		!bytes.Contains(response.Body.Bytes(), []byte(`"username":"employee"`)) ||
		!bytes.Contains(response.Body.Bytes(), []byte(`"device_name":"workstation-7"`)) {
		t.Fatalf("list status = %d, body = %s", response.Code, response.Body.String())
	}

	response = httptest.NewRecorder()
	handler.ServeHTTP(response, httptest.NewRequest(http.MethodPost, "/v1/admin/enrollments/enrollment-1/reject", nil))
	if response.Code != http.StatusOK || !bytes.Contains(response.Body.Bytes(), []byte(`"status":"rejected"`)) {
		t.Fatalf("reject status = %d, body = %s", response.Code, response.Body.String())
	}

	response = httptest.NewRecorder()
	handler.ServeHTTP(response, httptest.NewRequest(http.MethodPost, "/v1/admin/devices/device-1/revoke", nil))
	if response.Code != http.StatusOK || !bytes.Contains(response.Body.Bytes(), []byte(`"status":"revoked"`)) {
		t.Fatalf("revoke device status = %d, body = %s", response.Code, response.Body.String())
	}

	response = httptest.NewRecorder()
	handler.ServeHTTP(response, httptest.NewRequest(http.MethodGet, "/v1/admin/enrollments?status=unknown", nil))
	if response.Code != http.StatusBadRequest {
		t.Fatalf("invalid filter status = %d", response.Code)
	}
}

func signedCSR(t *testing.T) string {
	t.Helper()
	key, err := ecdsa.GenerateKey(elliptic.P256(), rand.Reader)
	if err != nil {
		t.Fatal(err)
	}
	der, err := x509.CreateCertificateRequest(rand.Reader, &x509.CertificateRequest{}, key)
	if err != nil {
		t.Fatal(err)
	}
	return string(pem.EncodeToMemory(&pem.Block{Type: "CERTIFICATE REQUEST", Bytes: der}))
}
