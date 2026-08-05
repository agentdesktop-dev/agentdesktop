package deviceauthorization

import (
	"context"
	"testing"

	"github.com/agentdesktop-dev/agentdesktop/control-plane/internal/deviceidentity"
	"github.com/agentdesktop-dev/agentdesktop/control-plane/internal/enrollment"
)

type recordingStore struct {
	principal enrollment.Principal
	device    deviceidentity.Identity
}

func (store *recordingStore) AuthorizeDevice(
	_ context.Context,
	principal enrollment.Principal,
	device deviceidentity.Identity,
) error {
	store.principal = principal
	store.device = device
	return nil
}

func TestAuthorizePassesVerifiedIdentitiesToStore(t *testing.T) {
	store := &recordingStore{}
	principal := enrollment.Principal{Issuer: "issuer", Subject: "user"}
	device := deviceidentity.Identity{OrganizationID: "organization", DeviceID: "device", SerialNumber: "01"}
	if err := NewService(store).Authorize(t.Context(), principal, device); err != nil {
		t.Fatal(err)
	}
	if store.principal != principal || store.device != device {
		t.Fatalf("store principal = %#v, device = %#v", store.principal, store.device)
	}
}
