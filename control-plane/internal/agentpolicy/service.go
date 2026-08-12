package agentpolicy

import (
	"context"
	"errors"

	"github.com/agentdesktop-dev/agentdesktop/control-plane/internal/enrollment"
)

var (
	ErrInvalidPolicy = errors.New("invalid agent policy")
	ErrNotFound      = errors.New("agent policy not found")
)

var AgentIDs = []string{"claude-code", "claude-desktop", "codex-cli", "openclaw", "vscode-copilot"}

type Store interface {
	PutAgentPolicy(context.Context, enrollment.Principal, Request) (Policy, error)
	GetAgentPolicy(context.Context, enrollment.Principal) (Policy, error)
}

type Service struct {
	store Store
}

func NewService(store Store) *Service {
	return &Service{store: store}
}

func (service *Service) Get(ctx context.Context, administrator enrollment.Principal) (Policy, error) {
	if service == nil || service.store == nil || administrator.Issuer == "" || administrator.Subject == "" {
		return Policy{}, ErrInvalidPolicy
	}
	policy, err := service.store.GetAgentPolicy(ctx, administrator)
	if errors.Is(err, ErrNotFound) {
		return Default(), nil
	}
	return policy, err
}

func (service *Service) Put(ctx context.Context, administrator enrollment.Principal, request Request) (Policy, error) {
	if service == nil || service.store == nil || administrator.Issuer == "" || administrator.Subject == "" || !valid(request) {
		return Policy{}, ErrInvalidPolicy
	}
	return service.store.PutAgentPolicy(ctx, administrator, request)
}

func Default() Policy {
	rules := make([]Rule, 0, len(AgentIDs))
	for _, agentID := range AgentIDs {
		rules = append(rules, Rule{AgentID: agentID, Action: "allow"})
	}
	return Policy{SchemaVersion: SchemaVersion, Rules: rules, Enforcement: "not_available"}
}

func valid(request Request) bool {
	if request.SchemaVersion != SchemaVersion || len(request.Rules) != len(AgentIDs) {
		return false
	}
	allowed := make(map[string]bool, len(AgentIDs))
	for _, agentID := range AgentIDs {
		allowed[agentID] = true
	}
	seen := make(map[string]bool, len(request.Rules))
	for _, rule := range request.Rules {
		if !allowed[rule.AgentID] || seen[rule.AgentID] || (rule.Action != "allow" && rule.Action != "deny") {
			return false
		}
		seen[rule.AgentID] = true
	}
	return true
}
