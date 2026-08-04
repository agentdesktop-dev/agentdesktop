package postgres_test

import (
	"context"
	"crypto/ecdsa"
	"crypto/elliptic"
	"crypto/rand"
	"crypto/x509"
	"encoding/pem"
	"os"
	"testing"

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
