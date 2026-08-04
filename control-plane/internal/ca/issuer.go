package ca

import (
	"context"
	"time"
)

type Identity struct {
	OrganizationID string
	DeviceID       string
}

type Certificate struct {
	ChainPEM     string
	NotAfter     time.Time
	NotBefore    time.Time
	SerialNumber string
}

type Issuer interface {
	Issue(context.Context, Identity, []byte) (Certificate, error)
}
