package postgres_test

import (
	"context"
	"crypto/ecdsa"
	"crypto/elliptic"
	"crypto/rand"
	"crypto/x509"
	"encoding/pem"
	"errors"
	"os"
	"testing"
	"time"

	"github.com/agentdesktop-dev/agentdesktop/control-plane/internal/certificate"
	"github.com/agentdesktop-dev/agentdesktop/control-plane/internal/enrollment"
	"github.com/agentdesktop-dev/agentdesktop/control-plane/internal/identifier"
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
		enrollment.Principal{Issuer: issuer, Subject: "user-1"},
		request,
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
	var storedIssuer, storedSubject, storedStatus, storedFingerprint string
	var storedCSR []byte
	err = pool.QueryRow(ctx, `
		SELECT organizations.issuer, users.subject, enrollments.status,
		       enrollments.public_key_fingerprint, enrollments.csr_der
		FROM enrollments
		JOIN organizations ON organizations.id = enrollments.organization_id
		JOIN users ON users.id = enrollments.user_id
		WHERE enrollments.id = $1
	`, record.ID).Scan(&storedIssuer, &storedSubject, &storedStatus, &storedFingerprint, &storedCSR)
	if err != nil {
		t.Fatal(err)
	}
	if storedIssuer != issuer || storedSubject != "user-1" || storedStatus != "pending" ||
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
		NotBefore:    time.Unix(1_000, 0),
		NotAfter:     time.Unix(2_000, 0),
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

	rejectRequest := validRequest(t)
	rejectEnrollmentID, err := identifier.New()
	if err != nil {
		t.Fatal(err)
	}
	rejectRecord, err := store.CreatePending(
		ctx,
		enrollment.Principal{Issuer: issuer, Subject: "user-2"},
		rejectRequest,
		rejectEnrollmentID,
	)
	if err != nil {
		t.Fatal(err)
	}
	pendingRecords, err := store.List(ctx, administrator, "pending", 100)
	if err != nil {
		t.Fatal(err)
	}
	if len(pendingRecords) != 1 || pendingRecords[0].EnrollmentID != rejectRecord.ID || pendingRecords[0].Subject != "user-2" {
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
	if len(approvedRecords) != 1 || approvedRecords[0].DeviceID != deviceID {
		t.Fatalf("approved administrator records = %#v", approvedRecords)
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
