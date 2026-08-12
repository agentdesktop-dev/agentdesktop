package agentpolicy

import "time"

const SchemaVersion = 1

type Rule struct {
	AgentID string `json:"agent_id"`
	Action  string `json:"action"`
}

type Request struct {
	SchemaVersion int    `json:"schema_version"`
	Rules         []Rule `json:"rules"`
}

type Policy struct {
	SchemaVersion int       `json:"schema_version"`
	Rules         []Rule    `json:"rules"`
	Configured    bool      `json:"configured"`
	Enforcement   string    `json:"enforcement"`
	UpdatedBy     string    `json:"updated_by,omitempty"`
	UpdatedAt     time.Time `json:"updated_at,omitempty"`
}
