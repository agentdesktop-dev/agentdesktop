package enrollment

import (
	"context"
	"errors"
	"strings"

	"github.com/agentdesktop-dev/agentdesktop/control-plane/internal/ca"
	"github.com/agentdesktop-dev/agentdesktop/control-plane/internal/certificate"
	"github.com/agentdesktop-dev/agentdesktop/control-plane/internal/identifier"
)

var (
	ErrInvalidPrincipal = errors.New("invalid authenticated principal")
	ErrIssuanceFailed   = errors.New("certificate issuance failed")
	ErrNotPending       = errors.New("enrollment is not pending")
)

type Store interface {
	CreatePending(context.Context, Principal, certificate.Request, string) (Enrollment, error)
	BeginIssuance(context.Context, Principal, string, string) (Issuance, error)
	AbortIssuance(context.Context, Principal, Issuance) error
	CompleteIssuance(context.Context, Principal, Issuance, IssuedCertificate) (Approval, error)
}

type Service struct {
	issuer ca.Issuer
	store  Store
}

func NewService(store Store, issuer ca.Issuer) *Service {
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
	issued, err := service.issuer.Issue(ctx, ca.Identity{
		OrganizationID: issuance.OrganizationID,
		DeviceID:       issuance.DeviceID,
	}, issuance.CSRDER)
	if err != nil {
		if abortErr := service.store.AbortIssuance(ctx, administrator, issuance); abortErr != nil {
			return Approval{}, errors.Join(ErrIssuanceFailed, err, abortErr)
		}
		return Approval{}, errors.Join(ErrIssuanceFailed, err)
	}
	return service.store.CompleteIssuance(ctx, administrator, issuance, IssuedCertificate{
		ChainPEM:     issued.ChainPEM,
		NotAfter:     issued.NotAfter,
		NotBefore:    issued.NotBefore,
		SerialNumber: issued.SerialNumber,
	})
}
