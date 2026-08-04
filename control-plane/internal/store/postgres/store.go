package postgres

import (
	"context"
	"errors"
	"time"

	"github.com/agentdesktop-dev/agentdesktop/control-plane/internal/certificate"
	"github.com/agentdesktop-dev/agentdesktop/control-plane/internal/enrollment"
	"github.com/agentdesktop-dev/agentdesktop/control-plane/internal/identifier"
	"github.com/jackc/pgx/v5"
	"github.com/jackc/pgx/v5/pgxpool"
)

type Store struct {
	pool *pgxpool.Pool
}

func Open(ctx context.Context, databaseURL string) (*Store, error) {
	pool, err := pgxpool.New(ctx, databaseURL)
	if err != nil {
		return nil, err
	}
	if err := pool.Ping(ctx); err != nil {
		pool.Close()
		return nil, err
	}
	return &Store{pool: pool}, nil
}

func (store *Store) Close() {
	store.pool.Close()
}

func (store *Store) EnsureOrganization(ctx context.Context, id, issuer, displayName string) error {
	_, err := store.pool.Exec(ctx, `
		INSERT INTO organizations (id, issuer, display_name)
		VALUES ($1, $2, $3)
		ON CONFLICT (id) DO UPDATE SET
			issuer = EXCLUDED.issuer,
			display_name = EXCLUDED.display_name
	`, id, issuer, displayName)
	return err
}

func (store *Store) CreatePending(
	ctx context.Context,
	principal enrollment.Principal,
	request certificate.Request,
	enrollmentID string,
) (enrollment.Enrollment, error) {
	transaction, err := store.pool.BeginTx(ctx, pgx.TxOptions{})
	if err != nil {
		return enrollment.Enrollment{}, err
	}
	defer transaction.Rollback(ctx)

	organizationID, err := findOrganization(ctx, transaction, principal.Issuer)
	if err != nil {
		return enrollment.Enrollment{}, err
	}
	userID, err := upsertUser(ctx, transaction, organizationID, principal.Subject)
	if err != nil {
		return enrollment.Enrollment{}, err
	}
	createdAt := time.Now().UTC()
	var storedEnrollmentID string
	var storedCreatedAt time.Time
	err = transaction.QueryRow(ctx, `
		INSERT INTO enrollments (
			id, organization_id, user_id, status, csr_der,
			public_key_fingerprint, created_at, updated_at
		) VALUES ($1, $2, $3, 'pending', $4, $5, $6, $6)
		ON CONFLICT (organization_id, user_id, public_key_fingerprint)
		WHERE status = 'pending'
		DO UPDATE SET updated_at = EXCLUDED.updated_at
		RETURNING id, created_at
	`, enrollmentID, organizationID, userID, request.DER, request.PublicKeyFingerprint, createdAt).Scan(
		&storedEnrollmentID,
		&storedCreatedAt,
	)
	if err != nil {
		return enrollment.Enrollment{}, err
	}
	auditID, err := identifier.New()
	if err != nil {
		return enrollment.Enrollment{}, err
	}
	_, err = transaction.Exec(ctx, `
		INSERT INTO audit_events (id, organization_id, actor_subject, action, target_id)
		VALUES ($1, $2, $3, 'enrollment.requested', $4)
	`, auditID, organizationID, principal.Subject, storedEnrollmentID)
	if err != nil {
		return enrollment.Enrollment{}, err
	}
	if err := transaction.Commit(ctx); err != nil {
		return enrollment.Enrollment{}, err
	}
	return enrollment.Enrollment{
		ID:                   storedEnrollmentID,
		Status:               "pending",
		Issuer:               principal.Issuer,
		Subject:              principal.Subject,
		PublicKeyFingerprint: request.PublicKeyFingerprint,
		CreatedAt:            storedCreatedAt,
	}, nil
}

func (store *Store) BeginIssuance(
	ctx context.Context,
	administrator enrollment.Principal,
	enrollmentID string,
	deviceID string,
) (enrollment.Issuance, error) {
	transaction, err := store.pool.BeginTx(ctx, pgx.TxOptions{})
	if err != nil {
		return enrollment.Issuance{}, err
	}
	defer transaction.Rollback(ctx)
	organizationID, err := findOrganization(ctx, transaction, administrator.Issuer)
	if err != nil {
		return enrollment.Issuance{}, err
	}
	if _, err := transaction.Exec(ctx, `
		INSERT INTO devices (id, organization_id, status)
		VALUES ($1, $2, 'active')
	`, deviceID, organizationID); err != nil {
		return enrollment.Issuance{}, err
	}
	issuance := enrollment.Issuance{
		EnrollmentID:   enrollmentID,
		OrganizationID: organizationID,
		DeviceID:       deviceID,
	}
	err = transaction.QueryRow(ctx, `
		UPDATE enrollments
		SET status = 'issuing', device_id = $1, updated_at = now()
		WHERE id = $2 AND organization_id = $3 AND status = 'pending'
		RETURNING csr_der, public_key_fingerprint
	`, deviceID, enrollmentID, organizationID).Scan(
		&issuance.CSRDER,
		&issuance.PublicKeyFingerprint,
	)
	if errors.Is(err, pgx.ErrNoRows) {
		return enrollment.Issuance{}, enrollment.ErrNotPending
	}
	if err != nil {
		return enrollment.Issuance{}, err
	}
	if err := insertAudit(ctx, transaction, organizationID, administrator.Subject, "enrollment.issuance_started", enrollmentID); err != nil {
		return enrollment.Issuance{}, err
	}
	if err := transaction.Commit(ctx); err != nil {
		return enrollment.Issuance{}, err
	}
	return issuance, nil
}

func (store *Store) AbortIssuance(
	ctx context.Context,
	administrator enrollment.Principal,
	issuance enrollment.Issuance,
) error {
	transaction, err := store.pool.BeginTx(ctx, pgx.TxOptions{})
	if err != nil {
		return err
	}
	defer transaction.Rollback(ctx)
	organizationID, err := findOrganization(ctx, transaction, administrator.Issuer)
	if err != nil {
		return err
	}
	result, err := transaction.Exec(ctx, `
		UPDATE enrollments
		SET status = 'pending', device_id = NULL, updated_at = now()
		WHERE id = $1 AND organization_id = $2 AND device_id = $3 AND status = 'issuing'
	`, issuance.EnrollmentID, organizationID, issuance.DeviceID)
	if err != nil {
		return err
	}
	if result.RowsAffected() != 1 {
		return enrollment.ErrNotPending
	}
	if _, err := transaction.Exec(ctx, `DELETE FROM devices WHERE id = $1 AND organization_id = $2`, issuance.DeviceID, organizationID); err != nil {
		return err
	}
	if err := insertAudit(ctx, transaction, organizationID, administrator.Subject, "enrollment.issuance_failed", issuance.EnrollmentID); err != nil {
		return err
	}
	return transaction.Commit(ctx)
}

func (store *Store) CompleteIssuance(
	ctx context.Context,
	administrator enrollment.Principal,
	issuance enrollment.Issuance,
	certificate enrollment.IssuedCertificate,
) (enrollment.Approval, error) {
	transaction, err := store.pool.BeginTx(ctx, pgx.TxOptions{})
	if err != nil {
		return enrollment.Approval{}, err
	}
	defer transaction.Rollback(ctx)
	organizationID, err := findOrganization(ctx, transaction, administrator.Issuer)
	if err != nil {
		return enrollment.Approval{}, err
	}
	if organizationID != issuance.OrganizationID {
		return enrollment.Approval{}, enrollment.ErrNotPending
	}
	if _, err := transaction.Exec(ctx, `
		INSERT INTO certificates (
			serial_number, organization_id, device_id, public_key_fingerprint,
			certificate_pem, not_before, not_after
		) VALUES ($1, $2, $3, $4, $5, $6, $7)
	`, certificate.SerialNumber, organizationID, issuance.DeviceID,
		issuance.PublicKeyFingerprint, certificate.ChainPEM,
		certificate.NotBefore, certificate.NotAfter); err != nil {
		return enrollment.Approval{}, err
	}
	result, err := transaction.Exec(ctx, `
		UPDATE enrollments
		SET status = 'approved', updated_at = now()
		WHERE id = $1 AND organization_id = $2 AND device_id = $3 AND status = 'issuing'
	`, issuance.EnrollmentID, organizationID, issuance.DeviceID)
	if err != nil {
		return enrollment.Approval{}, err
	}
	if result.RowsAffected() != 1 {
		return enrollment.Approval{}, enrollment.ErrNotPending
	}
	if err := insertAudit(ctx, transaction, organizationID, administrator.Subject, "enrollment.approved", issuance.EnrollmentID); err != nil {
		return enrollment.Approval{}, err
	}
	if err := transaction.Commit(ctx); err != nil {
		return enrollment.Approval{}, err
	}
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

func findOrganization(ctx context.Context, transaction pgx.Tx, issuer string) (string, error) {
	var id string
	err := transaction.QueryRow(ctx, `SELECT id FROM organizations WHERE issuer = $1`, issuer).Scan(&id)
	if errors.Is(err, pgx.ErrNoRows) {
		return "", errors.New("authenticated issuer is not configured")
	}
	return id, err
}

func upsertUser(ctx context.Context, transaction pgx.Tx, organizationID, subject string) (string, error) {
	id, err := identifier.New()
	if err != nil {
		return "", err
	}
	var storedID string
	err = transaction.QueryRow(ctx, `
		INSERT INTO users (id, organization_id, subject)
		VALUES ($1, $2, $3)
		ON CONFLICT (organization_id, subject)
		DO UPDATE SET subject = EXCLUDED.subject
		RETURNING id
	`, id, organizationID, subject).Scan(&storedID)
	return storedID, err
}

func insertAudit(
	ctx context.Context,
	transaction pgx.Tx,
	organizationID string,
	actorSubject string,
	action string,
	targetID string,
) error {
	auditID, err := identifier.New()
	if err != nil {
		return err
	}
	_, err = transaction.Exec(ctx, `
		INSERT INTO audit_events (id, organization_id, actor_subject, action, target_id)
		VALUES ($1, $2, $3, $4, $5)
	`, auditID, organizationID, actorSubject, action, targetID)
	return err
}
