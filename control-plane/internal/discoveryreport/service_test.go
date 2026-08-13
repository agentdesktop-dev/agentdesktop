package discoveryreport

import (
	"context"
	"encoding/json"
	"errors"
	"testing"

	"github.com/agentdesktop-dev/agentdesktop/control-plane/internal/deviceidentity"
	"github.com/agentdesktop-dev/agentdesktop/control-plane/internal/enrollment"
)

type recordingStore struct {
	putReport Report
	stored    StoredReport
	inventory InventoryPage
	devices   InventoryDevicePage
	rescan    RescanResult
	status    RescanStatus
	err       error
}

func (store *recordingStore) PutLatestDiscoveryReport(_ context.Context, _ deviceidentity.Identity, _ int, encoded []byte) (StoredReport, error) {
	if store.err != nil {
		return StoredReport{}, store.err
	}
	if err := json.Unmarshal(encoded, &store.putReport); err != nil {
		return StoredReport{}, err
	}
	return store.stored, nil
}

func (store *recordingStore) GetLatestDiscoveryReport(context.Context, enrollment.Principal, string) (StoredReport, error) {
	return store.stored, store.err
}

func (store *recordingStore) ListInventoryAssets(context.Context, enrollment.Principal, InventoryQuery) (InventoryPage, error) {
	return store.inventory, store.err
}

func (store *recordingStore) ListInventoryDevices(context.Context, enrollment.Principal, InventoryDeviceQuery) (InventoryDevicePage, error) {
	return store.devices, store.err
}

func (store *recordingStore) RequestDiscoveryRescan(context.Context, enrollment.Principal, RescanRequest) (RescanResult, error) {
	return store.rescan, store.err
}

func (store *recordingStore) GetDiscoveryRescanStatus(context.Context, deviceidentity.Identity) (RescanStatus, error) {
	return store.status, store.err
}

func TestServiceAcceptsBoundedMacOSReport(t *testing.T) {
	store := &recordingStore{stored: StoredReport{DeviceID: "device-1"}}
	service := NewService(store)
	report := validReport()
	stored, err := service.PutLatest(context.Background(), deviceidentity.Identity{
		OrganizationID: "org-1", UserID: "user-1", DeviceID: "device-1", SerialNumber: "01",
	}, report)
	if err != nil || stored.DeviceID != "device-1" || store.putReport.SchemaVersion != SchemaVersion {
		t.Fatalf("stored report = %#v, error = %v", stored, err)
	}
}

func TestServiceRejectsUnknownAgentsAndFields(t *testing.T) {
	report := validReport()
	report.Agents[0].ID = "unknown-agent"
	if _, err := NewService(&recordingStore{}).PutLatest(context.Background(), deviceidentity.Identity{
		OrganizationID: "org-1", UserID: "user-1", DeviceID: "device-1", SerialNumber: "01",
	}, report); !errors.Is(err, ErrInvalidReport) {
		t.Fatalf("unknown agent error = %v", err)
	}
}

func TestServiceRejectsUnsafeAgentVersion(t *testing.T) {
	report := validReport()
	unsafeVersion := "$(printenv SECRET)"
	report.Agents[0].Version = &unsafeVersion
	if _, err := NewService(&recordingStore{}).PutLatest(context.Background(), deviceidentity.Identity{
		OrganizationID: "org-1", UserID: "user-1", DeviceID: "device-1", SerialNumber: "01",
	}, report); !errors.Is(err, ErrInvalidReport) {
		t.Fatalf("unsafe version error = %v", err)
	}
}

func TestServiceValidatesPagedInventoryQueries(t *testing.T) {
	administrator := enrollment.Principal{Issuer: "https://issuer.example/", Subject: "admin-1"}
	store := &recordingStore{inventory: InventoryPage{Total: 3}, devices: InventoryDevicePage{Total: 2}}
	service := NewService(store)
	assets, err := service.Inventory(context.Background(), administrator, InventoryQuery{Kind: "agent", Search: "claude", Limit: 50})
	if err != nil || assets.Total != 3 {
		t.Fatalf("inventory = %#v, error = %v", assets, err)
	}
	devices, err := service.InventoryDevices(context.Background(), administrator, InventoryDeviceQuery{Kind: "agent", Key: "claude-code", Version: "2.1.4", Limit: 50})
	if err != nil || devices.Total != 2 {
		t.Fatalf("inventory devices = %#v, error = %v", devices, err)
	}
	for _, query := range []InventoryQuery{
		{Kind: "unknown", Limit: 50},
		{Kind: "agent", Limit: 0},
		{Kind: "agent", Limit: 101},
		{Kind: "agent", Search: "bad\nsearch", Limit: 50},
	} {
		if _, err := service.Inventory(context.Background(), administrator, query); !errors.Is(err, ErrInvalidReport) {
			t.Fatalf("invalid inventory query %#v error = %v", query, err)
		}
	}
	if _, err := service.InventoryDevices(context.Background(), administrator, InventoryDeviceQuery{Key: "claude-code", Limit: 50}); !errors.Is(err, ErrInvalidReport) {
		t.Fatalf("unscoped asset device query error = %v", err)
	}
}

func TestServiceValidatesRescanTargets(t *testing.T) {
	administrator := enrollment.Principal{Issuer: "issuer", Subject: "admin"}
	store := &recordingStore{rescan: RescanResult{Requested: 2}, status: RescanStatus{Pending: true}}
	service := NewService(store)
	result, err := service.RequestRescan(context.Background(), administrator, RescanRequest{
		TargetMode: "selected", DeviceIDs: []string{"11111111-1111-4111-8111-111111111111"},
	})
	if err != nil || result.Requested != 2 {
		t.Fatalf("rescan result = %#v, error = %v", result, err)
	}
	for _, request := range []RescanRequest{
		{TargetMode: "all_active", DeviceIDs: []string{"11111111-1111-4111-8111-111111111111"}},
		{TargetMode: "selected"},
		{TargetMode: "selected", DeviceIDs: []string{"invalid"}},
	} {
		if _, err := service.RequestRescan(context.Background(), administrator, request); !errors.Is(err, ErrInvalidRescan) {
			t.Fatalf("invalid rescan %#v error = %v", request, err)
		}
	}
	status, err := service.RescanStatus(context.Background(), deviceidentity.Identity{
		OrganizationID: "org", UserID: "user", DeviceID: "device", SerialNumber: "01",
	})
	if err != nil || !status.Pending {
		t.Fatalf("rescan status = %#v, error = %v", status, err)
	}
}

func validReport() Report {
	version := "2.1.4"
	return Report{
		SchemaVersion: SchemaVersion, CollectorVersion: "0.1.0", Platform: "macos",
		Coverage: Coverage{ProjectScopes: "not_scanned"},
		Agents: []Agent{{
			ID: "claude-code", Installed: true, Version: &version, Running: "detected", Evidence: []string{"executable"},
			ConfigSources: []ConfigSource{{Scope: "user", Source: "claude-settings", Format: "json", Status: "parsed", Sections: []string{"mcp"}}},
			MCPServers:    []MCPServer{{Name: "github", Scope: "user", Transport: "stdio"}},
			Skills:        []NamedResource{{Name: "review-pr", Scope: "user"}},
		}},
	}
}

func TestWindowsClaudeCodeReportIsValid(t *testing.T) {
	report := validReport()
	report.Platform = "windows"
	report.Agents[0].Running = "unknown"

	if err := validate(report); err != nil {
		t.Fatalf("validate Windows Claude Code report: %v", err)
	}
}
