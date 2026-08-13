package discoveryreport

import (
	"context"
	"encoding/json"
	"errors"
	"strings"
	"unicode"
	"unicode/utf8"

	"github.com/agentdesktop-dev/agentdesktop/control-plane/internal/deviceidentity"
	"github.com/agentdesktop-dev/agentdesktop/control-plane/internal/enrollment"
)

var (
	ErrInvalidReport = errors.New("invalid discovery report")
	ErrNotActive     = errors.New("device is not active")
	ErrNotFound      = errors.New("discovery report not found")
	ErrInvalidRescan = errors.New("invalid discovery rescan request")
)

const (
	MaxBodyBytes = 64 << 10
	maxAgents    = 16
	maxResources = 128
	maxIssues    = 64
	maxNameRunes = 256
)

type Store interface {
	PutLatestDiscoveryReport(context.Context, deviceidentity.Identity, int, []byte) (StoredReport, error)
	GetLatestDiscoveryReport(context.Context, enrollment.Principal, string) (StoredReport, error)
	ListInventoryAssets(context.Context, enrollment.Principal, InventoryQuery) (InventoryPage, error)
	ListInventoryDevices(context.Context, enrollment.Principal, InventoryDeviceQuery) (InventoryDevicePage, error)
	RequestDiscoveryRescan(context.Context, enrollment.Principal, RescanRequest) (RescanResult, error)
	GetDiscoveryRescanStatus(context.Context, deviceidentity.Identity) (RescanStatus, error)
}

type Service struct {
	store Store
}

func NewService(store Store) *Service {
	return &Service{store: store}
}

func (service *Service) PutLatest(ctx context.Context, device deviceidentity.Identity, report Report) (StoredReport, error) {
	if service == nil || service.store == nil || device.OrganizationID == "" || device.UserID == "" ||
		device.DeviceID == "" || device.SerialNumber == "" || validate(report) != nil {
		return StoredReport{}, ErrInvalidReport
	}
	encoded, err := json.Marshal(report)
	if err != nil || len(encoded) > MaxBodyBytes {
		return StoredReport{}, ErrInvalidReport
	}
	return service.store.PutLatestDiscoveryReport(ctx, device, report.SchemaVersion, encoded)
}

func (service *Service) GetLatest(ctx context.Context, administrator enrollment.Principal, deviceID string) (StoredReport, error) {
	if service == nil || service.store == nil || administrator.Issuer == "" || administrator.Subject == "" || deviceID == "" {
		return StoredReport{}, ErrNotFound
	}
	return service.store.GetLatestDiscoveryReport(ctx, administrator, deviceID)
}

func (service *Service) Inventory(ctx context.Context, administrator enrollment.Principal, query InventoryQuery) (InventoryPage, error) {
	if service == nil || service.store == nil || !validAdministrator(administrator) ||
		!validInventoryKind(query.Kind) || !validSearch(query.Search) || !validPage(query.Limit, query.Offset) {
		return InventoryPage{}, ErrInvalidReport
	}
	return service.store.ListInventoryAssets(ctx, administrator, query)
}

func (service *Service) InventoryDevices(ctx context.Context, administrator enrollment.Principal, query InventoryDeviceQuery) (InventoryDevicePage, error) {
	if service == nil || service.store == nil || !validAdministrator(administrator) ||
		!validSearch(query.Search) || !validPage(query.Limit, query.Offset) {
		return InventoryDevicePage{}, ErrInvalidReport
	}
	if query.Kind == "" {
		if query.Key != "" || query.Version != "" || query.Detail != "" {
			return InventoryDevicePage{}, ErrInvalidReport
		}
	} else if !validInventoryAsset(query) {
		return InventoryDevicePage{}, ErrInvalidReport
	}
	return service.store.ListInventoryDevices(ctx, administrator, query)
}

func (service *Service) RequestRescan(ctx context.Context, administrator enrollment.Principal, request RescanRequest) (RescanResult, error) {
	if service == nil || service.store == nil || !validAdministrator(administrator) || !validRescanRequest(request) {
		return RescanResult{}, ErrInvalidRescan
	}
	return service.store.RequestDiscoveryRescan(ctx, administrator, request)
}

func (service *Service) RescanStatus(ctx context.Context, device deviceidentity.Identity) (RescanStatus, error) {
	if service == nil || service.store == nil || device.OrganizationID == "" || device.UserID == "" ||
		device.DeviceID == "" || device.SerialNumber == "" {
		return RescanStatus{}, ErrNotActive
	}
	return service.store.GetDiscoveryRescanStatus(ctx, device)
}

func validate(report Report) error {
	if report.SchemaVersion != SchemaVersion || !oneOf(report.Platform, "macos", "windows") ||
		!validText(report.CollectorVersion, 64) || report.Coverage.ProjectScopes != "not_scanned" ||
		len(report.Agents) > maxAgents || len(report.Issues) > maxIssues {
		return ErrInvalidReport
	}
	seenAgents := make(map[string]bool)
	for _, agent := range report.Agents {
		if !validAgentID(agent.ID) || seenAgents[agent.ID] || !oneOf(agent.Running, "detected", "not_detected", "unknown") || len(agent.Evidence) > 8 ||
			len(agent.ConfigSources) > 16 || len(agent.MCPServers) > maxResources ||
			len(agent.Skills) > maxResources || len(agent.Plugins) > maxResources {
			return ErrInvalidReport
		}
		if agent.Version != nil && !validVersion(*agent.Version) {
			return ErrInvalidReport
		}
		seenAgents[agent.ID] = true
		for _, evidence := range agent.Evidence {
			if !oneOf(evidence, "executable", "application", "extension", "configuration") {
				return ErrInvalidReport
			}
		}
		for _, source := range agent.ConfigSources {
			if !oneOf(source.Scope, "user", "managed") || !validSource(agent.ID, source.Source) ||
				!oneOf(source.Format, "json", "toml") || !oneOf(source.Status, "parsed", "invalid", "oversized", "symlink_skipped") ||
				len(source.Sections) > 16 {
				return ErrInvalidReport
			}
			for _, section := range source.Sections {
				if !oneOf(section, "agents", "mcp", "models", "plugins", "providers", "skills") {
					return ErrInvalidReport
				}
			}
		}
		for _, server := range agent.MCPServers {
			if !validText(server.Name, maxNameRunes) || !oneOf(server.Scope, "user", "managed") ||
				!oneOf(server.Transport, "stdio", "http", "sse", "unknown") {
				return ErrInvalidReport
			}
		}
		for _, skill := range agent.Skills {
			if !validText(skill.Name, maxNameRunes) || !oneOf(skill.Scope, "user", "shared") {
				return ErrInvalidReport
			}
		}
		for _, plugin := range agent.Plugins {
			if !validText(plugin.Name, maxNameRunes) || !oneOf(plugin.Scope, "user", "shared") ||
				!oneOf(plugin.State, "enabled", "configured", "unknown") {
				return ErrInvalidReport
			}
		}
	}
	for _, issue := range report.Issues {
		if issue.AgentID != "" && !validAgentID(issue.AgentID) || !oneOf(issue.Code, "invalid_config", "oversized_config", "symlink_skipped", "entry_limit_reached") {
			return ErrInvalidReport
		}
	}
	return nil
}

func validAgentID(value string) bool {
	return oneOf(value, "claude-code", "claude-desktop", "codex-cli", "openclaw", "vscode-copilot")
}

func validAdministrator(administrator enrollment.Principal) bool {
	return administrator.Issuer != "" && administrator.Subject != ""
}

func validInventoryKind(value string) bool {
	return oneOf(value, "agent", "mcp", "skill", "plugin")
}

func validInventoryAsset(query InventoryDeviceQuery) bool {
	if !validInventoryKind(query.Kind) || !validText(query.Key, maxNameRunes) {
		return false
	}
	switch query.Kind {
	case "agent":
		return validAgentID(query.Key) && query.Detail == "" && (query.Version == "" || validVersion(query.Version))
	case "mcp":
		return query.Version == "" && (query.Detail == "" || oneOf(query.Detail, "stdio", "http", "sse", "unknown"))
	case "plugin":
		return query.Version == "" && (query.Detail == "" || oneOf(query.Detail, "enabled", "configured", "unknown"))
	case "skill":
		return query.Version == "" && query.Detail == ""
	default:
		return false
	}
}

func validSearch(value string) bool {
	return utf8.RuneCountInString(value) <= 256 && strings.IndexFunc(value, unicode.IsControl) < 0
}

func validPage(limit, offset int) bool {
	return limit > 0 && limit <= 100 && offset >= 0
}

func validRescanRequest(request RescanRequest) bool {
	if request.TargetMode == "all_active" {
		return len(request.DeviceIDs) == 0
	}
	if request.TargetMode != "selected" || len(request.DeviceIDs) == 0 || len(request.DeviceIDs) > 1000 {
		return false
	}
	seen := make(map[string]bool, len(request.DeviceIDs))
	for _, deviceID := range request.DeviceIDs {
		if !validUUID(deviceID) || seen[deviceID] {
			return false
		}
		seen[deviceID] = true
	}
	return true
}

func validUUID(value string) bool {
	if len(value) != 36 {
		return false
	}
	for index, character := range value {
		if index == 8 || index == 13 || index == 18 || index == 23 {
			if character != '-' {
				return false
			}
			continue
		}
		if !((character >= '0' && character <= '9') || (character >= 'a' && character <= 'f') || (character >= 'A' && character <= 'F')) {
			return false
		}
	}
	return true
}

func validSource(agentID, source string) bool {
	switch agentID {
	case "claude-code":
		return oneOf(source, "claude-settings", "claude-user-config", "claude-managed-mcp")
	case "claude-desktop":
		return source == "claude-desktop-extensions"
	case "codex-cli":
		return source == "codex-config"
	case "openclaw":
		return source == "openclaw-config"
	case "vscode-copilot":
		return oneOf(source, "vscode-user-mcp", "vscode-profile-mcp", "copilot-mcp")
	default:
		return false
	}
}

func validText(value string, limit int) bool {
	value = strings.TrimSpace(value)
	return value != "" && utf8.RuneCountInString(value) <= limit && strings.IndexFunc(value, unicode.IsControl) < 0
}

func validVersion(value string) bool {
	if len(value) == 0 || len(value) > 64 || value[0] < '0' || value[0] > '9' {
		return false
	}
	for _, character := range value {
		if !(character >= '0' && character <= '9') && !(character >= 'A' && character <= 'Z') &&
			!(character >= 'a' && character <= 'z') && character != '.' && character != '+' &&
			character != '-' && character != '_' {
			return false
		}
	}
	return true
}

func oneOf(value string, allowed ...string) bool {
	for _, candidate := range allowed {
		if value == candidate {
			return true
		}
	}
	return false
}
