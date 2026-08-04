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
