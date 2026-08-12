package postgres_test

import (
	"context"
	"crypto/ecdsa"
	"crypto/elliptic"
	"crypto/rand"
	"crypto/x509"
	"encoding/json"
	"encoding/pem"
	"errors"
	"os"
	"testing"
	"time"

	"github.com/agentdesktop-dev/agentdesktop/control-plane/internal/agentpolicy"
	"github.com/agentdesktop-dev/agentdesktop/control-plane/internal/certificate"
	"github.com/agentdesktop-dev/agentdesktop/control-plane/internal/deviceidentity"
	"github.com/agentdesktop-dev/agentdesktop/control-plane/internal/discoveryreport"
	"github.com/agentdesktop-dev/agentdesktop/control-plane/internal/enrollment"
	"github.com/agentdesktop-dev/agentdesktop/control-plane/internal/identifier"
	"github.com/agentdesktop-dev/agentdesktop/control-plane/internal/renewal"
	"github.com/agentdesktop-dev/agentdesktop/control-plane/internal/store/postgres"
	"github.com/agentdesktop-dev/agentdesktop/control-plane/migrations"
	"github.com/jackc/pgx/v5/pgxpool"
)

func TestCreatePendingPersistsAuthenticatedIdentityAndCSR(t *testing.T) {
	databaseURL := os.Getenv("TEST_DATABASE_URL")
	if databaseURL == "" {
		t.Skip("TEST_DATABASE_URL is not set")
	}
	ctx := context.Background()
	if err := migrations.Apply(ctx, databaseURL); err != nil {
		t.Fatal(err)
	}
	store, err := postgres.Open(ctx, databaseURL)
	if err != nil {
		t.Fatal(err)
	}
	defer store.Close()
	organizationID, err := identifier.New()
	if err != nil {
		t.Fatal(err)
	}
	issuer := "https://issuer.example/" + organizationID
	if err := store.EnsureOrganization(ctx, organizationID, issuer, "Test Organization"); err != nil {
		t.Fatal(err)
	}
	request := validRequest(t)
	enrollmentID, err := identifier.New()
	if err != nil {
		t.Fatal(err)
	}
	record, err := store.CreatePending(
		ctx,
		enrollment.Principal{Issuer: issuer, Subject: "user-1", DisplayName: "employee"},
		request,
		"workstation-7",
		enrollmentID,
	)
	if err != nil {
		t.Fatal(err)
	}
	if record.Status != "pending" || record.PublicKeyFingerprint != request.PublicKeyFingerprint {
		t.Fatalf("record = %#v", record)
	}
	pending, err := store.Get(ctx, enrollment.Principal{Issuer: issuer, Subject: "user-1"}, record.ID)
	if err != nil {
		t.Fatal(err)
	}
	if pending.Status != "pending" || pending.Certificate != nil {
		t.Fatalf("pending status = %#v", pending)
	}
	if _, err := store.Get(ctx, enrollment.Principal{Issuer: issuer, Subject: "user-2"}, record.ID); !errors.Is(err, enrollment.ErrNotFound) {
		t.Fatalf("foreign retrieval error = %v, want ErrNotFound", err)
	}
	retryID, err := identifier.New()
	if err != nil {
		t.Fatal(err)
	}
	retried, err := store.CreatePending(
		ctx,
		enrollment.Principal{Issuer: issuer, Subject: "user-1"},
		request,
		"workstation-7",
		retryID,
	)
	if err != nil {
		t.Fatal(err)
	}
	if retried.ID != record.ID {
		t.Fatalf("retry enrollment ID = %s, want %s", retried.ID, record.ID)
	}

	pool, err := pgxpool.New(ctx, databaseURL)
	if err != nil {
		t.Fatal(err)
	}
	defer pool.Close()
	var storedIssuer, storedSubject, storedDisplayName, storedStatus, storedFingerprint string
	var storedCSR []byte
	err = pool.QueryRow(ctx, `
		SELECT organizations.issuer, users.subject, COALESCE(users.display_name, ''), enrollments.status,
		       enrollments.public_key_fingerprint, enrollments.csr_der
		FROM enrollments
		JOIN organizations ON organizations.id = enrollments.organization_id
		JOIN users ON users.id = enrollments.user_id
		WHERE enrollments.id = $1
	`, record.ID).Scan(&storedIssuer, &storedSubject, &storedDisplayName, &storedStatus, &storedFingerprint, &storedCSR)
	if err != nil {
		t.Fatal(err)
	}
	if storedIssuer != issuer || storedSubject != "user-1" || storedDisplayName != "employee" || storedStatus != "pending" ||
		storedFingerprint != request.PublicKeyFingerprint || string(storedCSR) != string(request.DER) {
		t.Fatal("persisted enrollment does not match validated input")
	}
	var auditCount int
	if err := pool.QueryRow(ctx, `
		SELECT count(*) FROM audit_events
		WHERE target_id = $1 AND action = 'enrollment.requested'
	`, record.ID).Scan(&auditCount); err != nil {
		t.Fatal(err)
	}
	if auditCount != 2 {
		t.Fatalf("audit event count = %d, want 2", auditCount)
	}

	administrator := enrollment.Principal{Issuer: issuer, Subject: "admin-1"}
	deviceID, err := identifier.New()
	if err != nil {
		t.Fatal(err)
	}
	issuance, err := store.BeginIssuance(ctx, administrator, record.ID, deviceID)
	if err != nil {
		t.Fatal(err)
	}
	if issuance.DeviceID != deviceID || string(issuance.CSRDER) != string(request.DER) {
		t.Fatalf("issuance = %#v", issuance)
	}
	interrupted, err := store.ListIssuing(ctx, time.Now().Add(time.Minute), 10)
	if err != nil {
		t.Fatal(err)
	}
	if len(interrupted) != 1 || interrupted[0].EnrollmentID != record.ID ||
		interrupted[0].OrganizationIssuer != issuer || !interrupted[0].StartedAt.Equal(issuance.StartedAt) {
		t.Fatalf("interrupted issuances = %#v", interrupted)
	}
	issuing, err := store.Get(ctx, enrollment.Principal{Issuer: issuer, Subject: "user-1"}, record.ID)
	if err != nil {
		t.Fatal(err)
	}
	if issuing.Status != "issuing" || issuing.DeviceID != "" || issuing.Certificate != nil {
		t.Fatalf("issuing status exposed provisional credential = %#v", issuing)
	}
	issuingRecords, err := store.List(ctx, administrator, "issuing", 100)
	if err != nil {
		t.Fatal(err)
	}
	if len(issuingRecords) != 1 || issuingRecords[0].DeviceID != "" {
		t.Fatalf("administrator list exposed provisional device = %#v", issuingRecords)
	}
	if _, err := store.RevokeDevice(ctx, administrator, deviceID); !errors.Is(err, enrollment.ErrNotActive) {
		t.Fatalf("provisional device revocation error = %v, want ErrNotActive", err)
	}
	duplicateDeviceID, err := identifier.New()
	if err != nil {
		t.Fatal(err)
	}
	if _, err := store.BeginIssuance(ctx, administrator, record.ID, duplicateDeviceID); !errors.Is(err, enrollment.ErrNotPending) {
		t.Fatalf("duplicate claim error = %v, want ErrNotPending", err)
	}
	issued := enrollment.IssuedCertificate{
		ChainPEM:     "certificate-chain",
		SerialNumber: "01",
		NotBefore:    time.Now().Add(-time.Minute),
		NotAfter:     time.Now().Add(time.Hour),
	}
	approval, err := store.CompleteIssuance(ctx, administrator, issuance, issued)
	if err != nil {
		t.Fatal(err)
	}
	if approval.Status != "approved" || approval.DeviceID != deviceID || approval.CertificatePEM != issued.ChainPEM {
		t.Fatalf("approval = %#v", approval)
	}
	approved, err := store.Get(ctx, enrollment.Principal{Issuer: issuer, Subject: "user-1"}, record.ID)
	if err != nil {
		t.Fatal(err)
	}
	if approved.Status != "approved" || approved.DeviceID != deviceID ||
		approved.Certificate == nil || approved.Certificate.ChainPEM != issued.ChainPEM {
		t.Fatalf("approved status = %#v", approved)
	}
	var approvedStatus, storedDeviceID, storedCertificate string
	if err := pool.QueryRow(ctx, `
		SELECT enrollments.status, enrollments.device_id, certificates.certificate_pem
		FROM enrollments
		JOIN certificates ON certificates.device_id = enrollments.device_id
		WHERE enrollments.id = $1
	`, record.ID).Scan(&approvedStatus, &storedDeviceID, &storedCertificate); err != nil {
		t.Fatal(err)
	}
	if approvedStatus != "approved" || storedDeviceID != deviceID || storedCertificate != issued.ChainPEM {
		t.Fatal("approved enrollment or certificate was not persisted")
	}
	if err := pool.QueryRow(ctx, `
		SELECT count(*) FROM audit_events
		WHERE target_id = $1 AND action IN ('enrollment.issuance_started', 'enrollment.approved')
	`, record.ID).Scan(&auditCount); err != nil {
		t.Fatal(err)
	}
	if auditCount != 2 {
		t.Fatalf("approval audit event count = %d, want 2", auditCount)
	}

	renewalRequest := validRequest(t)
	renewalID, err := identifier.New()
	if err != nil {
		t.Fatal(err)
	}
	presentedDevice := deviceidentity.Identity{
		OrganizationID: organizationID,
		DeviceID:       deviceID,
		SerialNumber:   issued.SerialNumber,
	}
	owner := enrollment.Principal{Issuer: issuer, Subject: "user-1"}
	renewalClaim, err := store.Begin(ctx, owner, presentedDevice, renewalRequest, renewalID)
	if err != nil {
		t.Fatal(err)
	}
	if renewalClaim.ID != renewalID || renewalClaim.DeviceID != deviceID ||
		renewalClaim.PublicKeyFingerprint != renewalRequest.PublicKeyFingerprint || renewalClaim.Completed != nil {
		t.Fatalf("renewal claim = %#v", renewalClaim)
	}
	retryRenewalID, err := identifier.New()
	if err != nil {
		t.Fatal(err)
	}
	retriedClaim, err := store.Begin(ctx, owner, presentedDevice, renewalRequest, retryRenewalID)
	if err != nil {
		t.Fatal(err)
	}
	if retriedClaim.ID != renewalClaim.ID || !retriedClaim.StartedAt.Equal(renewalClaim.StartedAt) {
		t.Fatalf("retried renewal claim = %#v, want %#v", retriedClaim, renewalClaim)
	}
	if _, err := store.Begin(
		ctx,
		enrollment.Principal{Issuer: issuer, Subject: "user-2", DisplayName: "reviewer"},
		presentedDevice,
		validRequest(t),
		retryRenewalID,
	); !errors.Is(err, renewal.ErrNotActive) {
		t.Fatalf("foreign owner renewal error = %v, want ErrNotActive", err)
	}
	renewedCertificate := renewal.Certificate{
		ChainPEM: "renewed-certificate-chain", SerialNumber: "02",
		NotBefore: time.Now().Add(-time.Minute), NotAfter: time.Now().Add(-time.Second),
	}
	renewed, err := store.Complete(ctx, owner, renewalClaim, renewedCertificate)
	if err != nil {
		t.Fatal(err)
	}
	if renewed.Status != "approved" || renewed.DeviceID != deviceID ||
		renewed.Certificate.SerialNumber != renewedCertificate.SerialNumber {
		t.Fatalf("renewal response = %#v", renewed)
	}
	repeatedCompletion, err := store.Complete(ctx, owner, renewalClaim, renewedCertificate)
	if err != nil {
		t.Fatal(err)
	}
	if repeatedCompletion.Certificate.SerialNumber != renewedCertificate.SerialNumber {
		t.Fatalf("repeated renewal completion = %#v", repeatedCompletion)
	}
	completedRetry, err := store.Begin(ctx, owner, presentedDevice, renewalRequest, retryRenewalID)
	if err != nil {
		t.Fatal(err)
	}
	if completedRetry.Completed == nil || completedRetry.Completed.SerialNumber != renewedCertificate.SerialNumber {
		t.Fatalf("completed renewal retry = %#v", completedRetry)
	}
	if _, err := store.Begin(ctx, owner, presentedDevice, validRequest(t), retryRenewalID); !errors.Is(err, renewal.ErrNotActive) {
		t.Fatalf("stale certificate new renewal error = %v, want ErrNotActive", err)
	}
	if err := pool.QueryRow(ctx, `
		SELECT count(*) FROM audit_events
		WHERE target_id = $1 AND action IN ('certificate.renewal_started', 'certificate.renewed')
	`, renewalClaim.ID).Scan(&auditCount); err != nil {
		t.Fatal(err)
	}
	if auditCount != 2 {
		t.Fatalf("renewal audit event count = %d, want 2", auditCount)
	}

	recoveryRequest := validRequest(t)
	challengeID, err := identifier.New()
	if err != nil {
		t.Fatal(err)
	}
	challenge, err := store.CreateRecoveryChallenge(
		ctx, owner, deviceID, renewedCertificate.SerialNumber, recoveryRequest,
		challengeID, []byte("recovery-nonce"), time.Now().Add(5*time.Minute),
	)
	if err != nil {
		t.Fatal(err)
	}
	loadedChallenge, err := store.GetRecoveryChallenge(ctx, owner, challenge.ID)
	if err != nil {
		t.Fatal(err)
	}
	if loadedChallenge.DeviceID != deviceID || loadedChallenge.CertificatePEM != renewedCertificate.ChainPEM ||
		loadedChallenge.PublicKeyFingerprint != recoveryRequest.PublicKeyFingerprint {
		t.Fatalf("recovery challenge = %#v", loadedChallenge)
	}
	if _, err := store.GetRecoveryChallenge(
		ctx, enrollment.Principal{Issuer: issuer, Subject: "user-2"}, challenge.ID,
	); !errors.Is(err, renewal.ErrNotActive) {
		t.Fatalf("foreign recovery challenge error = %v, want ErrNotActive", err)
	}
	recoveryRenewalID, err := identifier.New()
	if err != nil {
		t.Fatal(err)
	}
	recoveryClaim, err := store.BeginRecovery(ctx, owner, loadedChallenge, recoveryRenewalID)
	if err != nil {
		t.Fatal(err)
	}
	replayedClaim, err := store.BeginRecovery(ctx, owner, loadedChallenge, "unused-replay-id")
	if err != nil {
		t.Fatal(err)
	}
	if replayedClaim.ID != recoveryClaim.ID {
		t.Fatalf("replayed recovery claim = %#v, want %#v", replayedClaim, recoveryClaim)
	}
	recoveredCertificate := renewal.Certificate{
		ChainPEM: "recovered-certificate-chain", SerialNumber: "03",
		NotBefore: time.Now().Add(-time.Minute), NotAfter: time.Now().Add(2 * time.Hour),
	}
	recovered, err := store.Complete(ctx, owner, recoveryClaim, recoveredCertificate)
	if err != nil {
		t.Fatal(err)
	}
	if recovered.Certificate.SerialNumber != recoveredCertificate.SerialNumber {
		t.Fatalf("recovery response = %#v", recovered)
	}
	if err := pool.QueryRow(ctx, `
		SELECT count(*) FROM audit_events
		WHERE target_id = $1 AND action IN ('certificate.recovery_challenged', 'certificate.recovery_started')
	`, challenge.ID).Scan(&auditCount); err != nil {
		t.Fatal(err)
	}
	if auditCount != 2 {
		t.Fatalf("recovery audit event count = %d, want 2", auditCount)
	}
	rejectRequest := validRequest(t)
	rejectEnrollmentID, err := identifier.New()
	if err != nil {
		t.Fatal(err)
	}
	rejectRecord, err := store.CreatePending(
		ctx,
		enrollment.Principal{Issuer: issuer, Subject: "user-2", DisplayName: "reviewer"},
		rejectRequest,
		"review-device",
		rejectEnrollmentID,
	)
	if err != nil {
		t.Fatal(err)
	}
	pendingRecords, err := store.List(ctx, administrator, "pending", 100)
	if err != nil {
		t.Fatal(err)
	}
	if len(pendingRecords) != 1 || pendingRecords[0].EnrollmentID != rejectRecord.ID || pendingRecords[0].Subject != "user-2" ||
		pendingRecords[0].Username != "reviewer" || pendingRecords[0].DeviceName != "review-device" {
		t.Fatalf("pending administrator records = %#v", pendingRecords)
	}
	foreignOrganizationID, err := identifier.New()
	if err != nil {
		t.Fatal(err)
	}
	foreignIssuer := "https://issuer.example/" + foreignOrganizationID
	if err := store.EnsureOrganization(ctx, foreignOrganizationID, foreignIssuer, "Foreign Organization"); err != nil {
		t.Fatal(err)
	}
	foreignAdministrator := enrollment.Principal{Issuer: foreignIssuer, Subject: "foreign-admin"}
	approvedRecords, err := store.List(ctx, administrator, "approved", 100)
	if err != nil {
		t.Fatal(err)
	}
	if len(approvedRecords) != 1 || approvedRecords[0].DeviceID != deviceID || approvedRecords[0].Username != "employee" || approvedRecords[0].DeviceName != "workstation-7" {
		t.Fatalf("approved administrator records = %#v", approvedRecords)
	}
	devices, err := store.ListDevices(ctx, administrator, 100)
	if err != nil {
		t.Fatal(err)
	}
	if len(devices) != 1 || devices[0].DeviceID != deviceID || devices[0].DeviceName != "workstation-7" || devices[0].Status != "active" ||
		devices[0].Subject != "user-1" || devices[0].Username != "employee" || devices[0].CertificateCount != 3 ||
		devices[0].RenewalCount != 2 || devices[0].CurrentCertificateSerialNumber == nil ||
		*devices[0].CurrentCertificateSerialNumber != recoveredCertificate.SerialNumber {
		t.Fatalf("administrator devices = %#v", devices)
	}
	var ownerUserID string
	if err := pool.QueryRow(ctx, `SELECT id FROM users WHERE organization_id = $1 AND subject = 'user-1'`, organizationID).Scan(&ownerUserID); err != nil {
		t.Fatal(err)
	}
	discoveryIdentity := deviceidentity.Identity{
		OrganizationID: organizationID,
		UserID:         ownerUserID,
		DeviceID:       deviceID,
		SerialNumber:   recoveredCertificate.SerialNumber,
	}
	discovery := discoveryreport.Report{
		SchemaVersion:    discoveryreport.SchemaVersion,
		CollectorVersion: "0.1.0",
		Platform:         "macos",
		Coverage:         discoveryreport.Coverage{ProjectScopes: "not_scanned"},
		Agents: []discoveryreport.Agent{{
			ID: "claude-code", Installed: true, Running: "detected", Evidence: []string{"executable"},
			MCPServers: []discoveryreport.MCPServer{{Name: "github", Scope: "user", Transport: "stdio"}},
			Skills:     []discoveryreport.NamedResource{{Name: "review-pr", Scope: "user"}},
			Plugins:    []discoveryreport.Plugin{{Name: "browser", Scope: "user", State: "enabled"}},
		}},
	}
	discoveryJSON, err := json.Marshal(discovery)
	if err != nil {
		t.Fatal(err)
	}
	staleIdentity := discoveryIdentity
	staleIdentity.SerialNumber = renewedCertificate.SerialNumber
	if _, err := store.PutLatestDiscoveryReport(ctx, staleIdentity, discovery.SchemaVersion, discoveryJSON); !errors.Is(err, discoveryreport.ErrNotActive) {
		t.Fatalf("stale certificate discovery report error = %v", err)
	}
	foreignIdentity := discoveryIdentity
	foreignIdentity.OrganizationID = foreignOrganizationID
	if _, err := store.PutLatestDiscoveryReport(ctx, foreignIdentity, discovery.SchemaVersion, discoveryJSON); !errors.Is(err, discoveryreport.ErrNotActive) {
		t.Fatalf("foreign certificate discovery report error = %v", err)
	}
	wrongUserIdentity := discoveryIdentity
	wrongUserIdentity.UserID = foreignOrganizationID
	if _, err := store.PutLatestDiscoveryReport(ctx, wrongUserIdentity, discovery.SchemaVersion, discoveryJSON); !errors.Is(err, discoveryreport.ErrNotActive) {
		t.Fatalf("wrong-user discovery report error = %v", err)
	}
	if _, err := store.PutLatestDiscoveryReport(ctx, discoveryIdentity, discovery.SchemaVersion, discoveryJSON); err != nil {
		t.Fatal(err)
	}
	rescan, err := store.RequestDiscoveryRescan(ctx, administrator, discoveryreport.RescanRequest{
		TargetMode: "selected", DeviceIDs: []string{deviceID},
	})
	if err != nil || rescan.Requested != 1 || rescan.RequestedAt.IsZero() {
		t.Fatalf("selected discovery rescan = %#v, error = %v", rescan, err)
	}
	rescanStatus, err := store.GetDiscoveryRescanStatus(ctx, discoveryIdentity)
	if err != nil || !rescanStatus.Pending || rescanStatus.RequestedAt == nil {
		t.Fatalf("pending discovery rescan = %#v, error = %v", rescanStatus, err)
	}
	if _, err := store.PutLatestDiscoveryReport(ctx, discoveryIdentity, discovery.SchemaVersion, discoveryJSON); err != nil {
		t.Fatal(err)
	}
	rescanStatus, err = store.GetDiscoveryRescanStatus(ctx, discoveryIdentity)
	if err != nil || rescanStatus.Pending {
		t.Fatalf("completed discovery rescan = %#v, error = %v", rescanStatus, err)
	}
	allRescan, err := store.RequestDiscoveryRescan(ctx, administrator, discoveryreport.RescanRequest{TargetMode: "all_active"})
	if err != nil || allRescan.Requested != 1 {
		t.Fatalf("all-active discovery rescan = %#v, error = %v", allRescan, err)
	}
	inventory, err := store.ListInventoryAssets(ctx, administrator, discoveryreport.InventoryQuery{Kind: "agent", Limit: 50})
	if err != nil || inventory.Counts.ActiveDevices != 1 || inventory.Counts.ReportingDevices != 1 ||
		inventory.Counts.Agents != 1 || inventory.Counts.MCPServers != 1 || inventory.Counts.Skills != 1 || inventory.Counts.Plugins != 1 ||
		inventory.Total != 1 || len(inventory.Assets) != 1 ||
		inventory.Assets[0].Key != "claude-code" || inventory.Assets[0].DeviceCount != 1 {
		t.Fatalf("agent inventory = %#v, error = %v", inventory, err)
	}
	for _, expectation := range []struct {
		kind, key, detail string
	}{
		{kind: "mcp", key: "github", detail: "stdio"},
		{kind: "skill", key: "review-pr"},
		{kind: "plugin", key: "browser", detail: "enabled"},
	} {
		assets, err := store.ListInventoryAssets(ctx, administrator, discoveryreport.InventoryQuery{Kind: expectation.kind, Limit: 50})
		if err != nil || assets.Total != 1 || len(assets.Assets) != 1 || assets.Assets[0].Key != expectation.key || assets.Assets[0].Detail != expectation.detail {
			t.Fatalf("%s inventory = %#v, error = %v", expectation.kind, assets, err)
		}
		devices, err := store.ListInventoryDevices(ctx, administrator, discoveryreport.InventoryDeviceQuery{
			Kind: expectation.kind, Key: expectation.key, Detail: expectation.detail, Limit: 50,
		})
		if err != nil || devices.Total != 1 || len(devices.Devices) != 1 || devices.Devices[0].DeviceID != deviceID {
			t.Fatalf("%s inventory devices = %#v, error = %v", expectation.kind, devices, err)
		}
	}
	inventoryDevices, err := store.ListInventoryDevices(ctx, administrator, discoveryreport.InventoryDeviceQuery{
		Kind: "agent", Key: "claude-code", Limit: 50,
	})
	if err != nil || inventoryDevices.Total != 1 || len(inventoryDevices.Devices) != 1 || inventoryDevices.Devices[0].DeviceID != deviceID {
		t.Fatalf("agent inventory devices = %#v, error = %v", inventoryDevices, err)
	}
	searchedDevices, err := store.ListInventoryDevices(ctx, administrator, discoveryreport.InventoryDeviceQuery{Search: "workstation", Limit: 50})
	if err != nil || searchedDevices.Total != 1 || len(searchedDevices.Devices) != 1 {
		t.Fatalf("searched inventory devices = %#v, error = %v", searchedDevices, err)
	}
	if _, err := store.GetAgentPolicy(ctx, administrator); !errors.Is(err, agentpolicy.ErrNotFound) {
		t.Fatalf("missing agent policy error = %v", err)
	}
	policyRequest := agentpolicy.Request{SchemaVersion: agentpolicy.SchemaVersion, Rules: agentpolicy.Default().Rules}
	policyRequest.Rules[2].Action = "deny"
	policy, err := store.PutAgentPolicy(ctx, administrator, policyRequest)
	if err != nil || !policy.Configured || policy.Rules[2].AgentID != "codex-cli" || policy.Rules[2].Action != "deny" {
		t.Fatalf("stored agent policy = %#v, error = %v", policy, err)
	}
	loadedPolicy, err := store.GetAgentPolicy(ctx, administrator)
	if err != nil || len(loadedPolicy.Rules) != 5 || loadedPolicy.Rules[2].Action != "deny" || loadedPolicy.Enforcement != "not_available" {
		t.Fatalf("loaded agent policy = %#v, error = %v", loadedPolicy, err)
	}
	if _, err := store.GetAgentPolicy(ctx, foreignAdministrator); !errors.Is(err, agentpolicy.ErrNotFound) {
		t.Fatalf("foreign agent policy error = %v", err)
	}
	policyRequest.Rules[2].Action = "allow"
	if _, err := store.PutAgentPolicy(ctx, administrator, policyRequest); err != nil {
		t.Fatal(err)
	}
	loadedPolicy, err = store.GetAgentPolicy(ctx, administrator)
	if err != nil || loadedPolicy.Rules[2].Action != "allow" {
		t.Fatalf("replaced agent policy = %#v, error = %v", loadedPolicy, err)
	}
	loadedDiscovery, err := store.GetLatestDiscoveryReport(ctx, administrator, deviceID)
	if err != nil || len(loadedDiscovery.Report.Agents) != 1 || loadedDiscovery.Report.Agents[0].ID != "claude-code" {
		t.Fatalf("discovery report = %#v, error = %v", loadedDiscovery, err)
	}
	if _, err := store.GetLatestDiscoveryReport(ctx, foreignAdministrator, deviceID); !errors.Is(err, discoveryreport.ErrNotFound) {
		t.Fatalf("foreign discovery report error = %v", err)
	}
	discovery.Agents[0].Installed = false
	discoveryJSON, _ = json.Marshal(discovery)
	if _, err := store.PutLatestDiscoveryReport(ctx, discoveryIdentity, discovery.SchemaVersion, discoveryJSON); err != nil {
		t.Fatal(err)
	}
	loadedDiscovery, err = store.GetLatestDiscoveryReport(ctx, administrator, deviceID)
	if err != nil || loadedDiscovery.Report.Agents[0].Installed {
		t.Fatalf("replacement discovery report = %#v, error = %v", loadedDiscovery, err)
	}
	summary, err := store.Summary(ctx, administrator)
	if err != nil {
		t.Fatal(err)
	}
	if summary.PendingEnrollments != 1 || summary.ApprovedEnrollments != 1 ||
		summary.ActiveDevices != 1 || summary.RevokedDevices != 0 ||
		summary.CertificatesExpiring24H != 1 || summary.Renewals24H != 2 || summary.GeneratedAt.IsZero() {
		t.Fatalf("fleet summary before revocation = %#v", summary)
	}
	if _, err := store.RevokeDevice(ctx, foreignAdministrator, deviceID); !errors.Is(err, enrollment.ErrNotActive) {
		t.Fatalf("foreign device revocation error = %v, want ErrNotActive", err)
	}
	revocation, err := store.RevokeDevice(ctx, administrator, deviceID)
	if err != nil {
		t.Fatal(err)
	}
	if revocation.Status != "revoked" || revocation.DeviceID != deviceID || revocation.RevokedAt.IsZero() {
		t.Fatalf("device revocation = %#v", revocation)
	}
	if _, err := store.RevokeDevice(ctx, administrator, deviceID); !errors.Is(err, enrollment.ErrNotActive) {
		t.Fatalf("repeat device revocation error = %v, want ErrNotActive", err)
	}
	if _, err := store.Begin(ctx, owner, presentedDevice, validRequest(t), retryRenewalID); !errors.Is(err, renewal.ErrNotActive) {
		t.Fatalf("revoked device renewal error = %v, want ErrNotActive", err)
	}
	if _, err := store.PutLatestDiscoveryReport(ctx, discoveryIdentity, discovery.SchemaVersion, discoveryJSON); !errors.Is(err, discoveryreport.ErrNotActive) {
		t.Fatalf("revoked device discovery report error = %v", err)
	}
	if retained, err := store.GetLatestDiscoveryReport(ctx, administrator, deviceID); err != nil || retained.DeviceID != deviceID {
		t.Fatalf("retained revoked-device discovery report = %#v, error = %v", retained, err)
	}
	var deviceStatus string
	var deviceRevokedAt, certificateRevokedAt time.Time
	if err := pool.QueryRow(ctx, `
		SELECT devices.status, devices.revoked_at, certificates.revoked_at
		FROM devices
		JOIN certificates ON certificates.device_id = devices.id
		WHERE devices.id = $1
	`, deviceID).Scan(&deviceStatus, &deviceRevokedAt, &certificateRevokedAt); err != nil {
		t.Fatal(err)
	}
	if deviceStatus != "revoked" || !deviceRevokedAt.Equal(revocation.RevokedAt) ||
		!certificateRevokedAt.Equal(revocation.RevokedAt) {
		t.Fatal("device and certificate revocation were not persisted atomically")
	}
	if err := pool.QueryRow(ctx, `
		SELECT count(*) FROM audit_events
		WHERE target_id = $1 AND action = 'device.revoked' AND actor_subject = 'admin-1'
	`, deviceID).Scan(&auditCount); err != nil {
		t.Fatal(err)
	}
	if auditCount != 1 {
		t.Fatalf("device revocation audit event count = %d, want 1", auditCount)
	}
	foreignRecords, err := store.List(ctx, foreignAdministrator, "pending", 100)
	if err != nil {
		t.Fatal(err)
	}
	if len(foreignRecords) != 0 {
		t.Fatalf("foreign administrator records = %#v, want none", foreignRecords)
	}
	if _, err := store.Reject(ctx, foreignAdministrator, rejectRecord.ID); !errors.Is(err, enrollment.ErrNotPending) {
		t.Fatalf("foreign rejection error = %v, want ErrNotPending", err)
	}
	rejected, err := store.Reject(ctx, administrator, rejectRecord.ID)
	if err != nil {
		t.Fatal(err)
	}
	if rejected.Status != "rejected" || rejected.Subject != "user-2" {
		t.Fatalf("rejected record = %#v", rejected)
	}
	if _, err := store.Reject(ctx, administrator, rejectRecord.ID); !errors.Is(err, enrollment.ErrNotPending) {
		t.Fatalf("repeat rejection error = %v, want ErrNotPending", err)
	}
	ownerStatus, err := store.Get(ctx, enrollment.Principal{Issuer: issuer, Subject: "user-2"}, rejectRecord.ID)
	if err != nil {
		t.Fatal(err)
	}
	if ownerStatus.Status != "rejected" || ownerStatus.DeviceID != "" || ownerStatus.Certificate != nil {
		t.Fatalf("owner rejection status = %#v", ownerStatus)
	}
	if err := pool.QueryRow(ctx, `
		SELECT count(*) FROM audit_events
		WHERE target_id = $1 AND action = 'enrollment.rejected' AND actor_subject = 'admin-1'
	`, rejectRecord.ID).Scan(&auditCount); err != nil {
		t.Fatal(err)
	}
	if auditCount != 1 {
		t.Fatalf("rejection audit event count = %d, want 1", auditCount)
	}
	summary, err = store.Summary(ctx, administrator)
	if err != nil {
		t.Fatal(err)
	}
	if summary.PendingEnrollments != 0 || summary.RejectedEnrollments != 1 ||
		summary.ActiveDevices != 0 || summary.RevokedDevices != 1 ||
		summary.CertificatesExpiring24H != 0 {
		t.Fatalf("fleet summary after revocation = %#v", summary)
	}

}

func validRequest(t *testing.T) certificate.Request {
	t.Helper()
	key, err := ecdsa.GenerateKey(elliptic.P256(), rand.Reader)
	if err != nil {
		t.Fatal(err)
	}
	der, err := x509.CreateCertificateRequest(rand.Reader, &x509.CertificateRequest{}, key)
	if err != nil {
		t.Fatal(err)
	}
	encoded := pem.EncodeToMemory(&pem.Block{Type: "CERTIFICATE REQUEST", Bytes: der})
	request, err := certificate.ParseRequest(string(encoded))
	if err != nil {
		t.Fatal(err)
	}
	return request
}
