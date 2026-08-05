package deviceauthorization

import (
	"context"
	"errors"

	"github.com/agentdesktop-dev/agentdesktop/control-plane/internal/deviceidentity"
	"github.com/agentdesktop-dev/agentdesktop/control-plane/internal/enrollment"
)

var (
	ErrInvalidRequest = errors.New("invalid device authorization request")
	ErrDenied         = errors.New("device authorization denied")
)

type Store interface {
	AuthorizeDevice(context.Context, enrollment.Principal, deviceidentity.Identity) error
}

type Service struct {
	store Store
}

func NewService(store Store) *Service {
	return &Service{store: store}
}

func (service *Service) Authorize(
	ctx context.Context,
	principal enrollment.Principal,
	device deviceidentity.Identity,
) error {
	if service.store == nil || principal.Issuer == "" || principal.Subject == "" ||
		device.OrganizationID == "" || device.DeviceID == "" || device.SerialNumber == "" {
		return ErrInvalidRequest
	}
	return service.store.AuthorizeDevice(ctx, principal, device)
}
