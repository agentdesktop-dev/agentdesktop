package discoveryreport

import "time"

const SchemaVersion = 1

type Report struct {
	SchemaVersion    int      `json:"schema_version"`
	CollectorVersion string   `json:"collector_version"`
	Platform         string   `json:"platform"`
	Coverage         Coverage `json:"coverage"`
	Agents           []Agent  `json:"agents"`
	Issues           []Issue  `json:"issues"`
}

type Coverage struct {
	ProjectScopes string `json:"project_scopes"`
	Partial       bool   `json:"partial"`
}

type Agent struct {
	ID            string          `json:"id"`
	Installed     bool            `json:"installed"`
	Version       *string         `json:"version"`
	Running       string          `json:"running"`
	Evidence      []string        `json:"evidence"`
	ConfigSources []ConfigSource  `json:"config_sources"`
	MCPServers    []MCPServer     `json:"mcp_servers"`
	Skills        []NamedResource `json:"skills"`
	Plugins       []Plugin        `json:"plugins"`
}

type ConfigSource struct {
	Scope    string   `json:"scope"`
	Source   string   `json:"source"`
	Format   string   `json:"format"`
	Status   string   `json:"status"`
	Sections []string `json:"sections"`
}

type MCPServer struct {
	Name      string `json:"name"`
	Scope     string `json:"scope"`
	Transport string `json:"transport"`
}

type NamedResource struct {
	Name  string `json:"name"`
	Scope string `json:"scope"`
}

type Plugin struct {
	Name  string `json:"name"`
	Scope string `json:"scope"`
	State string `json:"state"`
}

type Issue struct {
	AgentID string `json:"agent_id,omitempty"`
	Code    string `json:"code"`
}

type StoredReport struct {
	DeviceID   string    `json:"device_id"`
	ReceivedAt time.Time `json:"received_at"`
	Report     Report    `json:"report"`
}

type InventoryCounts struct {
	ActiveDevices    int64 `json:"active_devices"`
	ReportingDevices int64 `json:"reporting_devices"`
	Agents           int64 `json:"agents"`
	MCPServers       int64 `json:"mcp_servers"`
	Skills           int64 `json:"skills"`
	Plugins          int64 `json:"plugins"`
}

type InventoryAsset struct {
	Kind         string  `json:"kind"`
	Key          string  `json:"key"`
	Version      *string `json:"version,omitempty"`
	Detail       string  `json:"detail,omitempty"`
	DeviceCount  int64   `json:"device_count"`
	RunningCount int64   `json:"running_count,omitempty"`
}

type InventoryQuery struct {
	Kind   string
	Search string
	Limit  int
	Offset int
}

type InventoryPage struct {
	Counts      InventoryCounts  `json:"counts"`
	Kind        string           `json:"kind"`
	Assets      []InventoryAsset `json:"assets"`
	Total       int64            `json:"total"`
	Limit       int              `json:"limit"`
	Offset      int              `json:"offset"`
	GeneratedAt time.Time        `json:"generated_at"`
}

type InventoryDeviceQuery struct {
	Kind    string
	Key     string
	Version string
	Detail  string
	Search  string
	Limit   int
	Offset  int
}

type InventoryDevice struct {
	DeviceID         string     `json:"device_id"`
	DeviceName       string     `json:"device_name,omitempty"`
	Subject          string     `json:"subject"`
	Username         string     `json:"username,omitempty"`
	Status           string     `json:"status"`
	ReportReceivedAt *time.Time `json:"report_received_at"`
}

type InventoryDevicePage struct {
	Devices []InventoryDevice `json:"devices"`
	Total   int64             `json:"total"`
	Limit   int               `json:"limit"`
	Offset  int               `json:"offset"`
}

type RescanRequest struct {
	TargetMode string   `json:"target_mode"`
	DeviceIDs  []string `json:"device_ids"`
}

type RescanResult struct {
	Requested   int64     `json:"requested"`
	RequestedAt time.Time `json:"requested_at"`
}

type RescanStatus struct {
	Pending     bool       `json:"pending"`
	RequestedAt *time.Time `json:"requested_at,omitempty"`
}
