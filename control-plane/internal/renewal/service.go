package renewal

import (
	"context"
	"crypto/rand"
	"encoding/base64"
	"errors"
	"strings"
	"time"

	"github.com/agentdesktop-dev/agentdesktop/control-plane/internal/ca"
	"github.com/agentdesktop-dev/agentdesktop/control-plane/internal/certificate"
	"github.com/agentdesktop-dev/agentdesktop/control-plane/internal/deviceidentity"
	"github.com/agentdesktop-dev/agentdesktop/control-plane/internal/enrollment"
	"github.com/agentdesktop-dev/agentdesktop/control-plane/internal/identifier"
)

var (
	ErrInvalidRequest = errors.New("invalid renewal request")
	ErrNotActive      = errors.New("device or certificate is not active")
	ErrIssuanceFailed = errors.New("renewal certificate issuance failed")
)

type Store interface {
	Begin(context.Context, enrollment.Principal, deviceidentity.Identity, certificate.Request, string) (Claim, error)
	CreateRecoveryChallenge(context.Context, enrollment.Principal, string, string, certificate.Request, string, []byte, time.Time) (RecoveryChallenge, error)
	GetRecoveryChallenge(context.Context, enrollment.Principal, string) (RecoveryChallenge, error)
	BeginRecovery(context.Context, enrollment.Principal, RecoveryChallenge, string) (Claim, error)
	Complete(context.Context, enrollment.Principal, Claim, Certificate) (Response, error)
	ListIssuingRenewals(context.Context, time.Time, int) ([]Claim, error)
}

func (service *Service) CreateRecoveryChallenge(
	ctx context.Context,
	principal enrollment.Principal,
	deviceID string,
	presentedSerial string,
	encodedCSR string,
) (RecoveryChallengeResponse, error) {
	if service.issuer == nil || principal.Issuer == "" || principal.Subject == "" ||
		deviceID == "" || presentedSerial == "" || strings.TrimSpace(encodedCSR) == "" {
		return RecoveryChallengeResponse{}, ErrInvalidRequest
	}
	request, err := certificate.ParseRequest(encodedCSR)
	if err != nil {
		return RecoveryChallengeResponse{}, err
	}
	id, err := identifier.New()
	if err != nil {
		return RecoveryChallengeResponse{}, err
	}
	nonce := make([]byte, 32)
	if _, err := rand.Read(nonce); err != nil {
		return RecoveryChallengeResponse{}, err
	}
	challenge, err := service.store.CreateRecoveryChallenge(
		ctx, principal, deviceID, presentedSerial, request, id, nonce, time.Now().UTC().Add(5*time.Minute),
	)
	if err != nil {
		return RecoveryChallengeResponse{}, err
	}
	return RecoveryChallengeResponse{
		ChallengeID: challenge.ID, DeviceID: challenge.DeviceID,
		PublicKeyFingerprint: challenge.PublicKeyFingerprint,
		Nonce:                base64.RawURLEncoding.EncodeToString(challenge.Nonce), ExpiresAt: challenge.ExpiresAt,
	}, nil
}

func (service *Service) Recover(
	ctx context.Context,
	principal enrollment.Principal,
	challengeID string,
	proof string,
) (Response, error) {
	if service.issuer == nil || principal.Issuer == "" || principal.Subject == "" ||
		challengeID == "" || proof == "" {
		return Response{}, ErrInvalidRequest
	}
	challenge, err := service.store.GetRecoveryChallenge(ctx, principal, challengeID)
	if err != nil {
		return Response{}, err
	}
	if time.Now().UTC().After(challenge.ExpiresAt) {
		return Response{}, ErrNotActive
	}
	if err := VerifyRecoveryProof(challenge, proof); err != nil {
		return Response{}, err
	}
	renewalID, err := identifier.New()
	if err != nil {
		return Response{}, err
	}
	claim, err := service.store.BeginRecovery(ctx, principal, challenge, renewalID)
	if err != nil {
		return Response{}, err
	}
	if claim.Completed != nil {
		return responseFor(claim, *claim.Completed), nil
	}
	return service.issue(ctx, principal, claim)
}

type Service struct {
	issuer ca.RetryableIssuer
	store  Store
}

func NewService(store Store, issuer ca.RetryableIssuer) *Service {
	return &Service{store: store, issuer: issuer}
}

func (service *Service) Renew(
	ctx context.Context,
	principal enrollment.Principal,
	device deviceidentity.Identity,
	encodedCSR string,
) (Response, error) {
	if service.issuer == nil || principal.Issuer == "" || principal.Subject == "" ||
		device.OrganizationID == "" || device.DeviceID == "" || device.SerialNumber == "" ||
		strings.TrimSpace(encodedCSR) == "" {
		return Response{}, ErrInvalidRequest
	}
	request, err := certificate.ParseRequest(encodedCSR)
	if err != nil {
		return Response{}, err
	}
	id, err := identifier.New()
	if err != nil {
		return Response{}, err
	}
	claim, err := service.store.Begin(ctx, principal, device, request, id)
	if err != nil {
		return Response{}, err
	}
	if claim.Completed != nil {
		return responseFor(claim, *claim.Completed), nil
	}
	return service.issue(ctx, principal, claim)
}

func (service *Service) Reconcile(ctx context.Context, startedBefore time.Time, limit int) (int, error) {
	if service.issuer == nil || startedBefore.IsZero() || limit <= 0 {
		return 0, ErrInvalidRequest
	}
	claims, err := service.store.ListIssuingRenewals(ctx, startedBefore, limit)
	if err != nil {
		return 0, err
	}
	completed := 0
	var reconciliationErr error
	for _, claim := range claims {
		principal := enrollment.Principal{Issuer: claim.OrganizationIssuer, Subject: "system:renewal-reconciler"}
		if _, err := service.issue(ctx, principal, claim); err != nil {
			reconciliationErr = errors.Join(reconciliationErr, err)
			continue
		}
		completed++
	}
	return completed, reconciliationErr
}

func (service *Service) issue(ctx context.Context, principal enrollment.Principal, claim Claim) (Response, error) {
	issued, err := service.issuer.Issue(ctx, ca.IssuanceRequest{
		ID: claim.ID, CSRDER: claim.CSRDER, IssuedAt: claim.StartedAt,
		Identity: ca.Identity{
			OrganizationID: claim.OrganizationID,
			UserID: claim.UserID,
			DeviceID: claim.DeviceID,
		},
	})
	if err != nil {
		return Response{}, errors.Join(ErrIssuanceFailed, err)
	}
	return service.store.Complete(ctx, principal, claim, Certificate{
		ChainPEM: issued.ChainPEM, NotAfter: issued.NotAfter,
		NotBefore: issued.NotBefore, SerialNumber: issued.SerialNumber,
	})
}

func responseFor(claim Claim, certificate Certificate) Response {
	return Response{
		RenewalID: claim.ID, Status: "approved", DeviceID: claim.DeviceID,
		PublicKeyFingerprint: claim.PublicKeyFingerprint, Certificate: certificate,
	}
}
