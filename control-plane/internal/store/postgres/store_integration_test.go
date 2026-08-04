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

	abortRequest := validRequest(t)
	abortEnrollmentID, err := identifier.New()
	if err != nil {
		t.Fatal(err)
	}
	abortRecord, err := store.CreatePending(ctx, enrollment.Principal{Issuer: issuer, Subject: "user-1"}, abortRequest, abortEnrollmentID)
	if err != nil {
		t.Fatal(err)
	}
	abortDeviceID, err := identifier.New()
	if err != nil {
		t.Fatal(err)
	}
	abortedIssuance, err := store.BeginIssuance(ctx, administrator, abortRecord.ID, abortDeviceID)
	if err != nil {
		t.Fatal(err)
	}
	if err := store.AbortIssuance(ctx, administrator, abortedIssuance); err != nil {
		t.Fatal(err)
	}
	var abortedStatus string
	var abortedDeviceCount int
	if err := pool.QueryRow(ctx, `SELECT status FROM enrollments WHERE id = $1`, abortRecord.ID).Scan(&abortedStatus); err != nil {
		t.Fatal(err)
	}
	if err := pool.QueryRow(ctx, `SELECT count(*) FROM devices WHERE id = $1`, abortDeviceID).Scan(&abortedDeviceCount); err != nil {
		t.Fatal(err)
	}
	if abortedStatus != "pending" || abortedDeviceCount != 0 {
		t.Fatal("aborted issuance did not restore pending state")
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
