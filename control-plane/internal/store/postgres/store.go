package postgres

import (
	"bytes"
	"context"
	"errors"
	"time"

	"github.com/agentdesktop-dev/agentdesktop/control-plane/internal/certificate"
	"github.com/agentdesktop-dev/agentdesktop/control-plane/internal/deviceidentity"
	"github.com/agentdesktop-dev/agentdesktop/control-plane/internal/enrollment"
	"github.com/agentdesktop-dev/agentdesktop/control-plane/internal/identifier"
	"github.com/agentdesktop-dev/agentdesktop/control-plane/internal/renewal"
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
		RETURNING user_id, csr_der, public_key_fingerprint, updated_at
	`, deviceID, enrollmentID, organizationID).Scan(
		&issuance.UserID,
		&issuance.CSRDER,
		&issuance.PublicKeyFingerprint,
		&issuance.StartedAt,
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
	if _, err := transaction.Exec(ctx, `
		UPDATE devices SET current_certificate_serial_number = $1
		WHERE id = $2 AND organization_id = $3 AND status = 'active'
	`, certificate.SerialNumber, issuance.DeviceID, organizationID); err != nil {
		return enrollment.Approval{}, err
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

func (store *Store) Begin(
	ctx context.Context,
	principal enrollment.Principal,
	device deviceidentity.Identity,
	request certificate.Request,
	renewalID string,
) (renewal.Claim, error) {
	transaction, err := store.pool.BeginTx(ctx, pgx.TxOptions{})
	if err != nil {
		return renewal.Claim{}, err
	}
	defer transaction.Rollback(ctx)
	organizationID, err := findOrganization(ctx, transaction, principal.Issuer)
	if err != nil {
		return renewal.Claim{}, err
	}
	if organizationID != device.OrganizationID {
		return renewal.Claim{}, renewal.ErrNotActive
	}
	var userID string
	err = transaction.QueryRow(ctx, `
		SELECT users.id
		FROM users
		JOIN enrollments ON enrollments.user_id = users.id
		JOIN devices ON devices.id = enrollments.device_id
		JOIN certificates ON certificates.device_id = devices.id
		WHERE users.organization_id = $1 AND users.subject = $2
		  AND enrollments.status = 'approved' AND devices.id = $3
		  AND devices.organization_id = $1 AND devices.status = 'active'
		  AND certificates.serial_number = $4
		  AND (
			devices.current_certificate_serial_number = certificates.serial_number
			OR EXISTS (
				SELECT 1 FROM certificate_renewals AS retry
				WHERE retry.device_id = devices.id AND retry.user_id = users.id
				  AND retry.presented_serial_number = certificates.serial_number
				  AND retry.public_key_fingerprint = $5
			)
		  )
		  AND certificates.organization_id = $1
		  AND certificates.revoked_at IS NULL AND certificates.not_after > now()
		LIMIT 1
	`, organizationID, principal.Subject, device.DeviceID, device.SerialNumber, request.PublicKeyFingerprint).Scan(&userID)
	if errors.Is(err, pgx.ErrNoRows) {
		return renewal.Claim{}, renewal.ErrNotActive
	}
	if err != nil {
		return renewal.Claim{}, err
	}
	createdAt := time.Now().UTC()
	claim := renewal.Claim{
		ID: renewalID, OrganizationID: organizationID, OrganizationIssuer: principal.Issuer,
		UserID: userID, DeviceID: device.DeviceID, CSRDER: request.DER,
		PublicKeyFingerprint: request.PublicKeyFingerprint, StartedAt: createdAt,
	}
	var insertedID string
	err = transaction.QueryRow(ctx, `
		INSERT INTO certificate_renewals (
			id, organization_id, user_id, device_id, presented_serial_number,
			status, csr_der, public_key_fingerprint, created_at, updated_at
		) VALUES ($1, $2, $3, $4, $5, 'issuing', $6, $7, $8, $8)
		ON CONFLICT (device_id, public_key_fingerprint) DO NOTHING
		RETURNING id
	`, renewalID, organizationID, userID, device.DeviceID, device.SerialNumber,
		request.DER, request.PublicKeyFingerprint, createdAt).Scan(&insertedID)
	inserted := err == nil
	if err != nil && !errors.Is(err, pgx.ErrNoRows) {
		return renewal.Claim{}, err
	}
	var status string
	var certificateSerial *string
	err = transaction.QueryRow(ctx, `
		SELECT id, csr_der, public_key_fingerprint, created_at, status, certificate_serial_number
		FROM certificate_renewals
		WHERE device_id = $1 AND public_key_fingerprint = $2
	`, device.DeviceID, request.PublicKeyFingerprint).Scan(
		&claim.ID, &claim.CSRDER, &claim.PublicKeyFingerprint, &claim.StartedAt,
		&status, &certificateSerial,
	)
	if err != nil {
		return renewal.Claim{}, err
	}
	if certificateSerial != nil {
		var completed renewal.Certificate
		err = transaction.QueryRow(ctx, `
			SELECT certificate_pem, not_before, not_after, serial_number
			FROM certificates WHERE serial_number = $1
		`, *certificateSerial).Scan(
			&completed.ChainPEM, &completed.NotBefore, &completed.NotAfter, &completed.SerialNumber,
		)
		if err != nil {
			return renewal.Claim{}, err
		}
		claim.Completed = &completed
	}
	if inserted {
		if err := insertAudit(ctx, transaction, organizationID, principal.Subject, "certificate.renewal_started", claim.ID); err != nil {
			return renewal.Claim{}, err
		}
	}
	if err := transaction.Commit(ctx); err != nil {
		return renewal.Claim{}, err
	}
	return claim, nil
}

func (store *Store) CreateRecoveryChallenge(
	ctx context.Context,
	principal enrollment.Principal,
	deviceID string,
	presentedSerial string,
	request certificate.Request,
	challengeID string,
	nonce []byte,
	expiresAt time.Time,
) (renewal.RecoveryChallenge, error) {
	transaction, err := store.pool.BeginTx(ctx, pgx.TxOptions{})
	if err != nil {
		return renewal.RecoveryChallenge{}, err
	}
	defer transaction.Rollback(ctx)
	organizationID, err := findOrganization(ctx, transaction, principal.Issuer)
	if err != nil {
		return renewal.RecoveryChallenge{}, err
	}
	var userID, certificatePEM string
	err = transaction.QueryRow(ctx, `
		SELECT users.id, certificates.certificate_pem
		FROM users
		JOIN enrollments ON enrollments.user_id = users.id
		JOIN devices ON devices.id = enrollments.device_id
		JOIN certificates ON certificates.device_id = devices.id
		WHERE users.organization_id = $1 AND users.subject = $2
		  AND enrollments.status = 'approved' AND devices.id = $3
		  AND devices.organization_id = $1 AND devices.status = 'active'
		  AND certificates.serial_number = $4
		  AND devices.current_certificate_serial_number = certificates.serial_number
		  AND certificates.organization_id = $1 AND certificates.revoked_at IS NULL
		  AND certificates.not_after <= now()
		  AND certificates.not_after > now() - interval '7 days'
		LIMIT 1
	`, organizationID, principal.Subject, deviceID, presentedSerial).Scan(&userID, &certificatePEM)
	if errors.Is(err, pgx.ErrNoRows) {
		return renewal.RecoveryChallenge{}, renewal.ErrNotActive
	}
	if err != nil {
		return renewal.RecoveryChallenge{}, err
	}
	challenge := renewal.RecoveryChallenge{
		ID: challengeID, OrganizationID: organizationID, OrganizationIssuer: principal.Issuer,
		UserID: userID, DeviceID: deviceID, PresentedSerialNumber: presentedSerial,
		CSRDER: request.DER, PublicKeyFingerprint: request.PublicKeyFingerprint,
		Nonce: nonce, CertificatePEM: certificatePEM, ExpiresAt: expiresAt,
	}
	_, err = transaction.Exec(ctx, `
		INSERT INTO certificate_recovery_challenges (
			id, organization_id, user_id, device_id, presented_serial_number,
			csr_der, public_key_fingerprint, nonce, expires_at
		) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
	`, challenge.ID, challenge.OrganizationID, challenge.UserID, challenge.DeviceID,
		challenge.PresentedSerialNumber, challenge.CSRDER, challenge.PublicKeyFingerprint,
		challenge.Nonce, challenge.ExpiresAt)
	if err != nil {
		return renewal.RecoveryChallenge{}, err
	}
	if err := insertAudit(ctx, transaction, organizationID, principal.Subject, "certificate.recovery_challenged", challenge.ID); err != nil {
		return renewal.RecoveryChallenge{}, err
	}
	if err := transaction.Commit(ctx); err != nil {
		return renewal.RecoveryChallenge{}, err
	}
	return challenge, nil
}

func (store *Store) GetRecoveryChallenge(
	ctx context.Context,
	principal enrollment.Principal,
	challengeID string,
) (renewal.RecoveryChallenge, error) {
	var challenge renewal.RecoveryChallenge
	err := store.pool.QueryRow(ctx, `
		SELECT challenges.id, challenges.organization_id, organizations.issuer,
		       challenges.user_id, challenges.device_id, challenges.presented_serial_number,
		       challenges.csr_der, challenges.public_key_fingerprint, challenges.nonce,
		       certificates.certificate_pem, challenges.expires_at, challenges.renewal_id
		FROM certificate_recovery_challenges AS challenges
		JOIN organizations ON organizations.id = challenges.organization_id
		JOIN users ON users.id = challenges.user_id
		JOIN devices ON devices.id = challenges.device_id
		JOIN certificates ON certificates.serial_number = challenges.presented_serial_number
		WHERE challenges.id = $1 AND organizations.issuer = $2
		  AND users.subject = $3 AND devices.status = 'active'
		  AND devices.current_certificate_serial_number = certificates.serial_number
		  AND certificates.revoked_at IS NULL
	`, challengeID, principal.Issuer, principal.Subject).Scan(
		&challenge.ID, &challenge.OrganizationID, &challenge.OrganizationIssuer,
		&challenge.UserID, &challenge.DeviceID, &challenge.PresentedSerialNumber,
		&challenge.CSRDER, &challenge.PublicKeyFingerprint, &challenge.Nonce,
		&challenge.CertificatePEM, &challenge.ExpiresAt, &challenge.RenewalID,
	)
	if errors.Is(err, pgx.ErrNoRows) {
		return renewal.RecoveryChallenge{}, renewal.ErrNotActive
	}
	return challenge, err
}

func (store *Store) BeginRecovery(
	ctx context.Context,
	principal enrollment.Principal,
	verified renewal.RecoveryChallenge,
	renewalID string,
) (renewal.Claim, error) {
	transaction, err := store.pool.BeginTx(ctx, pgx.TxOptions{})
	if err != nil {
		return renewal.Claim{}, err
	}
	defer transaction.Rollback(ctx)
	organizationID, err := findOrganization(ctx, transaction, principal.Issuer)
	if err != nil {
		return renewal.Claim{}, err
	}
	var challenge renewal.RecoveryChallenge
	err = transaction.QueryRow(ctx, `
		SELECT challenges.id, challenges.organization_id, challenges.user_id,
		       challenges.device_id, challenges.presented_serial_number, challenges.csr_der,
		       challenges.public_key_fingerprint, challenges.nonce, challenges.expires_at,
		       challenges.renewal_id
		FROM certificate_recovery_challenges AS challenges
		JOIN users ON users.id = challenges.user_id
		WHERE challenges.id = $1 AND challenges.organization_id = $2 AND users.subject = $3
		FOR UPDATE OF challenges
	`, verified.ID, organizationID, principal.Subject).Scan(
		&challenge.ID, &challenge.OrganizationID, &challenge.UserID, &challenge.DeviceID,
		&challenge.PresentedSerialNumber, &challenge.CSRDER, &challenge.PublicKeyFingerprint,
		&challenge.Nonce, &challenge.ExpiresAt, &challenge.RenewalID,
	)
	if errors.Is(err, pgx.ErrNoRows) || (err == nil && (time.Now().UTC().After(challenge.ExpiresAt) ||
		challenge.DeviceID != verified.DeviceID || challenge.PresentedSerialNumber != verified.PresentedSerialNumber ||
		challenge.PublicKeyFingerprint != verified.PublicKeyFingerprint || !bytes.Equal(challenge.Nonce, verified.Nonce) ||
		!bytes.Equal(challenge.CSRDER, verified.CSRDER))) {
		return renewal.Claim{}, renewal.ErrNotActive
	}
	if err != nil {
		return renewal.Claim{}, err
	}
	var active bool
	var currentSerial *string
	err = transaction.QueryRow(ctx, `
		SELECT status = 'active', current_certificate_serial_number
		FROM devices WHERE id = $1 AND organization_id = $2 FOR UPDATE
	`, challenge.DeviceID, organizationID).Scan(&active, &currentSerial)
	if errors.Is(err, pgx.ErrNoRows) || (err == nil && (!active || currentSerial == nil || *currentSerial != challenge.PresentedSerialNumber)) {
		return renewal.Claim{}, renewal.ErrNotActive
	}
	if err != nil {
		return renewal.Claim{}, err
	}
	claimID := renewalID
	if challenge.RenewalID == nil {
		_, err = transaction.Exec(ctx, `
			INSERT INTO certificate_renewals (
				id, organization_id, user_id, device_id, presented_serial_number,
				status, csr_der, public_key_fingerprint, created_at, updated_at
			) VALUES ($1, $2, $3, $4, $5, 'issuing', $6, $7, now(), now())
			ON CONFLICT (device_id, public_key_fingerprint) DO NOTHING
		`, claimID, organizationID, challenge.UserID, challenge.DeviceID,
			challenge.PresentedSerialNumber, challenge.CSRDER, challenge.PublicKeyFingerprint)
		if err != nil {
			return renewal.Claim{}, err
		}
		err = transaction.QueryRow(ctx, `
			SELECT id FROM certificate_renewals
			WHERE device_id = $1 AND public_key_fingerprint = $2
		`, challenge.DeviceID, challenge.PublicKeyFingerprint).Scan(&claimID)
		if err != nil {
			return renewal.Claim{}, err
		}
		_, err = transaction.Exec(ctx, `
			UPDATE certificate_recovery_challenges SET renewal_id = $1, used_at = now()
			WHERE id = $2 AND renewal_id IS NULL
		`, claimID, challenge.ID)
		if err != nil {
			return renewal.Claim{}, err
		}
		if err := insertAudit(ctx, transaction, organizationID, principal.Subject, "certificate.recovery_started", challenge.ID); err != nil {
			return renewal.Claim{}, err
		}
	} else {
		claimID = *challenge.RenewalID
	}
	claim := renewal.Claim{OrganizationID: organizationID, OrganizationIssuer: principal.Issuer}
	var certificateSerial *string
	err = transaction.QueryRow(ctx, `
		SELECT id, user_id, device_id, csr_der, public_key_fingerprint, created_at, certificate_serial_number
		FROM certificate_renewals WHERE id = $1 AND organization_id = $2
	`, claimID, organizationID).Scan(
		&claim.ID, &claim.UserID, &claim.DeviceID, &claim.CSRDER, &claim.PublicKeyFingerprint,
		&claim.StartedAt, &certificateSerial,
	)
	if err != nil {
		return renewal.Claim{}, err
	}
	if certificateSerial != nil {
		var completed renewal.Certificate
		err = transaction.QueryRow(ctx, `
			SELECT certificate_pem, not_before, not_after, serial_number
			FROM certificates WHERE serial_number = $1
		`, *certificateSerial).Scan(
			&completed.ChainPEM, &completed.NotBefore, &completed.NotAfter, &completed.SerialNumber,
		)
		if err != nil {
			return renewal.Claim{}, err
		}
		claim.Completed = &completed
	}
	if err := transaction.Commit(ctx); err != nil {
		return renewal.Claim{}, err
	}
	return claim, nil
}

func (store *Store) Complete(
	ctx context.Context,
	principal enrollment.Principal,
	claim renewal.Claim,
	certificate renewal.Certificate,
) (renewal.Response, error) {
	transaction, err := store.pool.BeginTx(ctx, pgx.TxOptions{})
	if err != nil {
		return renewal.Response{}, err
	}
	defer transaction.Rollback(ctx)
	organizationID, err := findOrganization(ctx, transaction, principal.Issuer)
	if err != nil {
		return renewal.Response{}, err
	}
	if organizationID != claim.OrganizationID {
		return renewal.Response{}, renewal.ErrNotActive
	}
	var active bool
	err = transaction.QueryRow(ctx, `
		SELECT status = 'active' FROM devices
		WHERE id = $1 AND organization_id = $2
		FOR UPDATE
	`, claim.DeviceID, organizationID).Scan(&active)
	if errors.Is(err, pgx.ErrNoRows) || (err == nil && !active) {
		return renewal.Response{}, renewal.ErrNotActive
	}
	if err != nil {
		return renewal.Response{}, err
	}
	var renewalStatus string
	err = transaction.QueryRow(ctx, `
		SELECT status FROM certificate_renewals
		WHERE id = $1 AND organization_id = $2 AND device_id = $3
		  AND public_key_fingerprint = $4
		FOR UPDATE
	`, claim.ID, organizationID, claim.DeviceID, claim.PublicKeyFingerprint).Scan(&renewalStatus)
	if errors.Is(err, pgx.ErrNoRows) {
		return renewal.Response{}, renewal.ErrNotActive
	}
	if err != nil {
		return renewal.Response{}, err
	}
	if renewalStatus == "issuing" {
		if _, err := transaction.Exec(ctx, `
		INSERT INTO certificates (
			serial_number, organization_id, device_id, public_key_fingerprint,
			certificate_pem, not_before, not_after
		) VALUES ($1, $2, $3, $4, $5, $6, $7)
	`, certificate.SerialNumber, organizationID, claim.DeviceID,
			claim.PublicKeyFingerprint, certificate.ChainPEM,
			certificate.NotBefore, certificate.NotAfter); err != nil {
			return renewal.Response{}, err
		}
		if _, err := transaction.Exec(ctx, `
			UPDATE devices SET current_certificate_serial_number = $1
			WHERE id = $2 AND organization_id = $3 AND status = 'active'
		`, certificate.SerialNumber, claim.DeviceID, organizationID); err != nil {
			return renewal.Response{}, err
		}
		result, err := transaction.Exec(ctx, `
		UPDATE certificate_renewals
		SET status = 'approved', certificate_serial_number = $1, updated_at = now()
		WHERE id = $2 AND organization_id = $3 AND device_id = $4 AND status = 'issuing'
	`, certificate.SerialNumber, claim.ID, organizationID, claim.DeviceID)
		if err != nil {
			return renewal.Response{}, err
		}
		if result.RowsAffected() != 1 {
			return renewal.Response{}, renewal.ErrNotActive
		}
		if err := insertAudit(ctx, transaction, organizationID, principal.Subject, "certificate.renewed", claim.ID); err != nil {
			return renewal.Response{}, err
		}
	}
	var response renewal.Response
	response.Certificate = renewal.Certificate{}
	err = transaction.QueryRow(ctx, `
		SELECT renewals.id, renewals.status, renewals.device_id,
		       renewals.public_key_fingerprint, certificates.certificate_pem,
		       certificates.not_before, certificates.not_after, certificates.serial_number
		FROM certificate_renewals AS renewals
		JOIN certificates ON certificates.serial_number = renewals.certificate_serial_number
		WHERE renewals.id = $1 AND renewals.organization_id = $2 AND renewals.device_id = $3
	`, claim.ID, organizationID, claim.DeviceID).Scan(
		&response.RenewalID, &response.Status, &response.DeviceID,
		&response.PublicKeyFingerprint, &response.Certificate.ChainPEM,
		&response.Certificate.NotBefore, &response.Certificate.NotAfter,
		&response.Certificate.SerialNumber,
	)
	if err != nil {
		return renewal.Response{}, err
	}
	if err := transaction.Commit(ctx); err != nil {
		return renewal.Response{}, err
	}
	return response, nil
}

func (store *Store) ListIssuingRenewals(
	ctx context.Context,
	startedBefore time.Time,
	limit int,
) ([]renewal.Claim, error) {
	rows, err := store.pool.Query(ctx, `
		SELECT renewals.id, renewals.organization_id, organizations.issuer,
		       renewals.user_id, renewals.device_id, renewals.csr_der,
		       renewals.public_key_fingerprint, renewals.created_at
		FROM certificate_renewals AS renewals
		JOIN organizations ON organizations.id = renewals.organization_id
		JOIN devices ON devices.id = renewals.device_id
		WHERE renewals.status = 'issuing' AND renewals.updated_at <= $1
		  AND devices.status = 'active'
		ORDER BY renewals.updated_at, renewals.id
		LIMIT $2
	`, startedBefore, limit)
	if err != nil {
		return nil, err
	}
	defer rows.Close()
	claims := make([]renewal.Claim, 0)
	for rows.Next() {
		var claim renewal.Claim
		if err := rows.Scan(
			&claim.ID, &claim.OrganizationID, &claim.OrganizationIssuer,
			&claim.UserID, &claim.DeviceID, &claim.CSRDER, &claim.PublicKeyFingerprint, &claim.StartedAt,
		); err != nil {
			return nil, err
		}
		claims = append(claims, claim)
	}
	return claims, rows.Err()
}

func (store *Store) ListIssuing(
	ctx context.Context,
	startedBefore time.Time,
	limit int,
) ([]enrollment.Issuance, error) {
	rows, err := store.pool.Query(ctx, `
		SELECT enrollments.id, enrollments.organization_id, organizations.issuer,
		       enrollments.user_id, enrollments.device_id, enrollments.csr_der,
		       enrollments.public_key_fingerprint, enrollments.updated_at
		FROM enrollments
		JOIN organizations ON organizations.id = enrollments.organization_id
		WHERE enrollments.status = 'issuing' AND enrollments.updated_at <= $1
		ORDER BY enrollments.updated_at, enrollments.id
		LIMIT $2
	`, startedBefore, limit)
	if err != nil {
		return nil, err
	}
	defer rows.Close()
	issuances := make([]enrollment.Issuance, 0)
	for rows.Next() {
		var issuance enrollment.Issuance
		if err := rows.Scan(
			&issuance.EnrollmentID,
			&issuance.OrganizationID,
			&issuance.OrganizationIssuer,
			&issuance.UserID,
			&issuance.DeviceID,
			&issuance.CSRDER,
			&issuance.PublicKeyFingerprint,
			&issuance.StartedAt,
		); err != nil {
			return nil, err
		}
		issuances = append(issuances, issuance)
	}
	return issuances, rows.Err()
}

func (store *Store) List(
	ctx context.Context,
	administrator enrollment.Principal,
	status string,
	limit int,
) ([]enrollment.AdministrativeRecord, error) {
	rows, err := store.pool.Query(ctx, `
		SELECT enrollments.id, enrollments.status, users.subject,
		       enrollments.public_key_fingerprint, enrollments.created_at,
		       enrollments.updated_at, enrollments.device_id
		FROM enrollments
		JOIN organizations ON organizations.id = enrollments.organization_id
		JOIN users ON users.id = enrollments.user_id
		WHERE organizations.issuer = $1 AND enrollments.status = $2
		ORDER BY enrollments.created_at, enrollments.id
		LIMIT $3
	`, administrator.Issuer, status, limit)
	if err != nil {
		return nil, err
	}
	defer rows.Close()
	records := make([]enrollment.AdministrativeRecord, 0)
	for rows.Next() {
		var record enrollment.AdministrativeRecord
		var deviceID *string
		if err := rows.Scan(
			&record.EnrollmentID,
			&record.Status,
			&record.Subject,
			&record.PublicKeyFingerprint,
			&record.CreatedAt,
			&record.UpdatedAt,
			&deviceID,
		); err != nil {
			return nil, err
		}
		if record.Status == "approved" && deviceID != nil {
			record.DeviceID = *deviceID
		}
		records = append(records, record)
	}
	return records, rows.Err()
}

func (store *Store) RevokeDevice(
	ctx context.Context,
	administrator enrollment.Principal,
	deviceID string,
) (enrollment.DeviceRevocation, error) {
	transaction, err := store.pool.BeginTx(ctx, pgx.TxOptions{})
	if err != nil {
		return enrollment.DeviceRevocation{}, err
	}
	defer transaction.Rollback(ctx)
	organizationID, err := findOrganization(ctx, transaction, administrator.Issuer)
	if err != nil {
		return enrollment.DeviceRevocation{}, err
	}
	var revocation enrollment.DeviceRevocation
	err = transaction.QueryRow(ctx, `
		UPDATE devices
		SET status = 'revoked', revoked_at = now()
		WHERE id = $1 AND organization_id = $2 AND status = 'active'
		  AND EXISTS (
			SELECT 1 FROM enrollments
			WHERE enrollments.device_id = devices.id
			  AND enrollments.organization_id = devices.organization_id
			  AND enrollments.status = 'approved'
		  )
		RETURNING id, status, revoked_at
	`, deviceID, organizationID).Scan(
		&revocation.DeviceID,
		&revocation.Status,
		&revocation.RevokedAt,
	)
	if errors.Is(err, pgx.ErrNoRows) {
		return enrollment.DeviceRevocation{}, enrollment.ErrNotActive
	}
	if err != nil {
		return enrollment.DeviceRevocation{}, err
	}
	if _, err := transaction.Exec(ctx, `
		UPDATE certificates
		SET revoked_at = $1
		WHERE organization_id = $2 AND device_id = $3 AND revoked_at IS NULL
	`, revocation.RevokedAt, organizationID, deviceID); err != nil {
		return enrollment.DeviceRevocation{}, err
	}
	if err := insertAudit(ctx, transaction, organizationID, administrator.Subject, "device.revoked", deviceID); err != nil {
		return enrollment.DeviceRevocation{}, err
	}
	if err := transaction.Commit(ctx); err != nil {
		return enrollment.DeviceRevocation{}, err
	}
	return revocation, nil
}

func (store *Store) Reject(
	ctx context.Context,
	administrator enrollment.Principal,
	enrollmentID string,
) (enrollment.AdministrativeRecord, error) {
	transaction, err := store.pool.BeginTx(ctx, pgx.TxOptions{})
	if err != nil {
		return enrollment.AdministrativeRecord{}, err
	}
	defer transaction.Rollback(ctx)
	organizationID, err := findOrganization(ctx, transaction, administrator.Issuer)
	if err != nil {
		return enrollment.AdministrativeRecord{}, err
	}
	var record enrollment.AdministrativeRecord
	err = transaction.QueryRow(ctx, `
		UPDATE enrollments
		SET status = 'rejected', updated_at = now()
		FROM users
		WHERE enrollments.id = $1 AND enrollments.organization_id = $2
		  AND enrollments.status = 'pending' AND users.id = enrollments.user_id
		RETURNING enrollments.id, enrollments.status, users.subject,
		          enrollments.public_key_fingerprint, enrollments.created_at,
		          enrollments.updated_at
	`, enrollmentID, organizationID).Scan(
		&record.EnrollmentID,
		&record.Status,
		&record.Subject,
		&record.PublicKeyFingerprint,
		&record.CreatedAt,
		&record.UpdatedAt,
	)
	if errors.Is(err, pgx.ErrNoRows) {
		return enrollment.AdministrativeRecord{}, enrollment.ErrNotPending
	}
	if err != nil {
		return enrollment.AdministrativeRecord{}, err
	}
	if err := insertAudit(ctx, transaction, organizationID, administrator.Subject, "enrollment.rejected", enrollmentID); err != nil {
		return enrollment.AdministrativeRecord{}, err
	}
	if err := transaction.Commit(ctx); err != nil {
		return enrollment.AdministrativeRecord{}, err
	}
	return record, nil
}

func (store *Store) Get(
	ctx context.Context,
	principal enrollment.Principal,
	enrollmentID string,
) (enrollment.Status, error) {
	var record enrollment.Status
	var deviceID, chainPEM, serialNumber *string
	var notBefore, notAfter *time.Time
	err := store.pool.QueryRow(ctx, `
		SELECT enrollments.id, enrollments.status,
		       enrollments.public_key_fingerprint, enrollments.created_at,
		       enrollments.device_id, latest_certificate.certificate_pem,
		       latest_certificate.serial_number, latest_certificate.not_before,
		       latest_certificate.not_after
		FROM enrollments
		JOIN organizations ON organizations.id = enrollments.organization_id
		JOIN users ON users.id = enrollments.user_id
		LEFT JOIN LATERAL (
			SELECT certificate_pem, serial_number, not_before, not_after
			FROM certificates
			WHERE certificates.device_id = enrollments.device_id
			ORDER BY not_after DESC
			LIMIT 1
		) AS latest_certificate ON true
		WHERE enrollments.id = $1 AND organizations.issuer = $2 AND users.subject = $3
	`, enrollmentID, principal.Issuer, principal.Subject).Scan(
		&record.EnrollmentID,
		&record.Status,
		&record.PublicKeyFingerprint,
		&record.CreatedAt,
		&deviceID,
		&chainPEM,
		&serialNumber,
		&notBefore,
		&notAfter,
	)
	if errors.Is(err, pgx.ErrNoRows) {
		return enrollment.Status{}, enrollment.ErrNotFound
	}
	if err != nil {
		return enrollment.Status{}, err
	}
	if record.Status == "approved" && deviceID != nil {
		record.DeviceID = *deviceID
	}
	if chainPEM != nil && serialNumber != nil && notBefore != nil && notAfter != nil {
		record.Certificate = &enrollment.IssuedCertificate{
			ChainPEM:     *chainPEM,
			SerialNumber: *serialNumber,
			NotBefore:    *notBefore,
			NotAfter:     *notAfter,
		}
	}
	return record, nil
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
