package agentpolicy

import (
	"context"
	"errors"
	"testing"

	"github.com/agentdesktop-dev/agentdesktop/control-plane/internal/enrollment"
)

type recordingStore struct {
	request Request
	policy  Policy
	err     error
}

func (store *recordingStore) PutAgentPolicy(_ context.Context, _ enrollment.Principal, request Request) (Policy, error) {
	store.request = request
	return store.policy, store.err
}

func (store *recordingStore) GetAgentPolicy(context.Context, enrollment.Principal) (Policy, error) {
	return store.policy, store.err
}

func TestServiceStoresCompleteAgentPolicy(t *testing.T) {
	request := Request{SchemaVersion: SchemaVersion, Rules: Default().Rules}
	request.Rules[2].Action = "deny"
	store := &recordingStore{policy: Policy{SchemaVersion: SchemaVersion, Rules: request.Rules, Configured: true}}
	policy, err := NewService(store).Put(context.Background(), enrollment.Principal{Issuer: "issuer", Subject: "admin"}, request)
	if err != nil || !policy.Configured || store.request.Rules[2].Action != "deny" {
		t.Fatalf("policy = %#v, request = %#v, error = %v", policy, store.request, err)
	}
}

func TestServiceDefaultsToAllowAndRejectsIncompletePolicies(t *testing.T) {
	administrator := enrollment.Principal{Issuer: "issuer", Subject: "admin"}
	policy, err := NewService(&recordingStore{err: ErrNotFound}).Get(context.Background(), administrator)
	if err != nil || policy.Configured || len(policy.Rules) != len(AgentIDs) {
		t.Fatalf("default policy = %#v, error = %v", policy, err)
	}
	invalid := []Request{
		{SchemaVersion: 2, Rules: Default().Rules},
		{SchemaVersion: SchemaVersion, Rules: Default().Rules[:4]},
		{SchemaVersion: SchemaVersion, Rules: append(Default().Rules[:4], Rule{AgentID: "unknown", Action: "allow"})},
		{SchemaVersion: SchemaVersion, Rules: append(Default().Rules[:4], Rule{AgentID: "vscode-copilot", Action: "audit"})},
	}
	for _, request := range invalid {
		if _, err := NewService(&recordingStore{}).Put(context.Background(), administrator, request); !errors.Is(err, ErrInvalidPolicy) {
			t.Fatalf("invalid request %#v error = %v", request, err)
		}
	}
}
