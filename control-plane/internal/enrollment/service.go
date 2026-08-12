package enrollment

import (
	"context"
	"errors"
	"strings"
	"time"
	"unicode"
	"unicode/utf8"

	"github.com/agentdesktop-dev/agentdesktop/control-plane/internal/ca"
	"github.com/agentdesktop-dev/agentdesktop/control-plane/internal/certificate"
	"github.com/agentdesktop-dev/agentdesktop/control-plane/internal/identifier"
)

var (
	ErrInvalidPrincipal  = errors.New("invalid authenticated principal")
	ErrIssuanceFailed    = errors.New("certificate issuance failed")
	ErrNotFound          = errors.New("enrollment not found")
	ErrNotPending        = errors.New("enrollment is not pending")
	ErrNotActive         = errors.New("device is not active")
	ErrInvalidDeviceName = errors.New("invalid device name")
)

const maxDeviceNameLength = 128

type Store interface {
	CreatePending(context.Context, Principal, certificate.Request, string, string) (Enrollment, error)
	BeginIssuance(context.Context, Principal, string, string) (Issuance, error)
	CompleteIssuance(context.Context, Principal, Issuance, IssuedCertificate) (Approval, error)
	Get(context.Context, Principal, string) (Status, error)
	List(context.Context, Principal, string, int) ([]AdministrativeRecord, error)
	ListDevices(context.Context, Principal, int) ([]AdministrativeDevice, error)
	Summary(context.Context, Principal) (FleetSummary, error)
	ListIssuing(context.Context, time.Time, int) ([]Issuance, error)
	Reject(context.Context, Principal, string) (AdministrativeRecord, error)
	RevokeDevice(context.Context, Principal, string) (DeviceRevocation, error)
}

type Service struct {
	issuer ca.RetryableIssuer
	store  Store
}

func NewService(store Store, issuer ca.RetryableIssuer) *Service {
	return &Service{store: store, issuer: issuer}
}

func (service *Service) Request(ctx context.Context, principal Principal, encodedCSR, deviceName string) (Enrollment, error) {
	if principal.Issuer == "" || principal.Subject == "" || strings.TrimSpace(encodedCSR) == "" {
		return Enrollment{}, ErrInvalidPrincipal
	}
	request, err := certificate.ParseRequest(encodedCSR)
	if err != nil {
		return Enrollment{}, err
	}
	deviceName, err = normalizeDeviceName(deviceName)
	if err != nil {
		return Enrollment{}, err
	}
	id, err := identifier.New()
	if err != nil {
		return Enrollment{}, err
	}
	return service.store.CreatePending(ctx, principal, request, deviceName, id)
}

func normalizeDeviceName(value string) (string, error) {
	value = strings.TrimSpace(value)
	if value == "" {
		return "", nil
	}
	if utf8.RuneCountInString(value) > maxDeviceNameLength || strings.IndexFunc(value, unicode.IsControl) >= 0 {
		return "", ErrInvalidDeviceName
	}
	return value, nil
}

func (service *Service) Get(ctx context.Context, principal Principal, enrollmentID string) (Status, error) {
	if principal.Issuer == "" || principal.Subject == "" || enrollmentID == "" {
		return Status{}, ErrInvalidPrincipal
	}
	return service.store.Get(ctx, principal, enrollmentID)
}

func (service *Service) List(
	ctx context.Context,
	administrator Principal,
	status string,
	limit int,
) ([]AdministrativeRecord, error) {
	if administrator.Issuer == "" || administrator.Subject == "" || limit <= 0 || limit > 100 ||
		!validAdministrativeStatus(status) {
		return nil, ErrInvalidPrincipal
	}
	return service.store.List(ctx, administrator, status, limit)
}

func (service *Service) Reject(
	ctx context.Context,
	administrator Principal,
	enrollmentID string,
) (AdministrativeRecord, error) {
	if administrator.Issuer == "" || administrator.Subject == "" || enrollmentID == "" {
		return AdministrativeRecord{}, ErrInvalidPrincipal
	}
	return service.store.Reject(ctx, administrator, enrollmentID)
}

func (service *Service) RevokeDevice(
	ctx context.Context,
	administrator Principal,
	deviceID string,
) (DeviceRevocation, error) {
	if administrator.Issuer == "" || administrator.Subject == "" || deviceID == "" {
		return DeviceRevocation{}, ErrInvalidPrincipal
	}
	return service.store.RevokeDevice(ctx, administrator, deviceID)
}

func (service *Service) ListDevices(
	ctx context.Context,
	administrator Principal,
	limit int,
) ([]AdministrativeDevice, error) {
	if administrator.Issuer == "" || administrator.Subject == "" || limit <= 0 || limit > 100 {
		return nil, ErrInvalidPrincipal
	}
	return service.store.ListDevices(ctx, administrator, limit)
}

func (service *Service) Summary(
	ctx context.Context,
	administrator Principal,
) (FleetSummary, error) {
	if administrator.Issuer == "" || administrator.Subject == "" {
		return FleetSummary{}, ErrInvalidPrincipal
	}
	return service.store.Summary(ctx, administrator)
}

func validAdministrativeStatus(status string) bool {
	return status == "pending" || status == "issuing" || status == "approved" || status == "rejected"
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
			UserID:         issuance.UserID,
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
