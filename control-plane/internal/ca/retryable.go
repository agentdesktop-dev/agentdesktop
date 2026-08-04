package ca

import (
	"context"
	"time"
)

type IssuanceRequest struct {
	ID       string
	Identity Identity
	CSRDER   []byte
	IssuedAt time.Time
}

type RetryableIssuer interface {
	Issue(context.Context, IssuanceRequest) (Certificate, error)
}
