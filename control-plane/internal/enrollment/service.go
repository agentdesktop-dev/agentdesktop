package enrollment

import (
	"context"
	"errors"
	"strings"
	"time"

	"github.com/agentdesktop-dev/agentdesktop/control-plane/internal/ca"
	"github.com/agentdesktop-dev/agentdesktop/control-plane/internal/certificate"
	"github.com/agentdesktop-dev/agentdesktop/control-plane/internal/identifier"
)

var (
	ErrInvalidPrincipal = errors.New("invalid authenticated principal")
	ErrIssuanceFailed   = errors.New("certificate issuance failed")
	ErrNotFound         = errors.New("enrollment not found")
	ErrNotPending       = errors.New("enrollment is not pending")
)

type Store interface {
	CreatePending(context.Context, Principal, certificate.Request, string) (Enrollment, error)
	BeginIssuance(context.Context, Principal, string, string) (Issuance, error)
	CompleteIssuance(context.Context, Principal, Issuance, IssuedCertificate) (Approval, error)
	Get(context.Context, Principal, string) (Status, error)
	ListIssuing(context.Context, time.Time, int) ([]Issuance, error)
}

type Service struct {
	issuer ca.RetryableIssuer
	store  Store
}

func NewService(store Store, issuer ca.RetryableIssuer) *Service {
	return &Service{store: store, issuer: issuer}
}

func (service *Service) Request(ctx context.Context, principal Principal, encodedCSR string) (Enrollment, error) {
	if principal.Issuer == "" || principal.Subject == "" || strings.TrimSpace(encodedCSR) == "" {
		return Enrollment{}, ErrInvalidPrincipal
	}
	request, err := certificate.ParseRequest(encodedCSR)
	if err != nil {
		return Enrollment{}, err
	}
	id, err := identifier.New()
	if err != nil {
		return Enrollment{}, err
	}
	return service.store.CreatePending(ctx, principal, request, id)
}

func (service *Service) Get(ctx context.Context, principal Principal, enrollmentID string) (Status, error) {
	if principal.Issuer == "" || principal.Subject == "" || enrollmentID == "" {
		return Status{}, ErrInvalidPrincipal
	}
	return service.store.Get(ctx, principal, enrollmentID)
}

func (service *Service) Approve(ctx context.Context, administrator Principal, enrollmentID string) (Approval, error) {
	if service.issuer == nil || administrator.Issuer == "" || administrator.Subject == "" || enrollmentID == "" {
		return Approval{}, ErrInvalidPrincipal
	}
	deviceID, err := identifier.New()
	if err != nil {
		return Approval{}, err
	}
	issuance, err := service.store.BeginIssuance(ctx, administrator, enrollmentID, deviceID)
	if err != nil {
		return Approval{}, err
	}
	return service.issue(ctx, administrator, issuance)
}

func (service *Service) Reconcile(ctx context.Context, startedBefore time.Time, limit int) (int, error) {
	if service.issuer == nil || startedBefore.IsZero() || limit <= 0 {
		return 0, ErrInvalidPrincipal
	}
	issuances, err := service.store.ListIssuing(ctx, startedBefore, limit)
	if err != nil {
		return 0, err
	}
	completed := 0
	var reconciliationErr error
	for _, issuance := range issuances {
		administrator := Principal{
			Issuer:  issuance.OrganizationIssuer,
			Subject: "system:issuance-reconciler",
		}
		if _, err := service.issue(ctx, administrator, issuance); err != nil {
			if !errors.Is(err, ErrNotPending) {
				reconciliationErr = errors.Join(reconciliationErr, err)
			}
			continue
		}
		completed++
	}
	return completed, reconciliationErr
}

func (service *Service) issue(
	ctx context.Context,
	administrator Principal,
	issuance Issuance,
) (Approval, error) {
	issued, err := service.issuer.Issue(ctx, ca.IssuanceRequest{
		ID:       issuance.EnrollmentID,
		CSRDER:   issuance.CSRDER,
		IssuedAt: issuance.StartedAt,
		Identity: ca.Identity{
			OrganizationID: issuance.OrganizationID,
			DeviceID:       issuance.DeviceID,
		},
	})
	if err != nil {
		return Approval{}, errors.Join(ErrIssuanceFailed, err)
	}
	return service.store.CompleteIssuance(ctx, administrator, issuance, IssuedCertificate{
		ChainPEM:     issued.ChainPEM,
		NotAfter:     issued.NotAfter,
		NotBefore:    issued.NotBefore,
		SerialNumber: issued.SerialNumber,
	})
}
