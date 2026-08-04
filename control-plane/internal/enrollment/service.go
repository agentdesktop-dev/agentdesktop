package enrollment

import (
	"context"
	"errors"
	"strings"

	"github.com/agentdesktop-dev/agentdesktop/control-plane/internal/certificate"
	"github.com/agentdesktop-dev/agentdesktop/control-plane/internal/identifier"
)

var ErrInvalidPrincipal = errors.New("invalid authenticated principal")

type Store interface {
	CreatePending(context.Context, Principal, certificate.Request, string) (Enrollment, error)
}

type Service struct {
	store Store
}

func NewService(store Store) *Service {
	return &Service{store: store}
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
