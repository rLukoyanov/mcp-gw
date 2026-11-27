package gateway

import (
	"errors"
	"testing"

	"github.com/modelcontextprotocol/go-sdk/mcp"
	"github.com/rLukoyanov/tokenmaxing/internal/config"
)

// fixtureRegistry builds a small indexed registry shared by the gateway tests.
func fixtureRegistry() *Registry {
	registry := NewRegistry()

	registry.AddServer("filesystem", config.TransportStdio, "Local filesystem access", true)
	registry.MarkConnected("filesystem", []*mcp.Tool{
		{
			Name:        "read_file",
			Description: "Read a file from disk",
			InputSchema: map[string]any{"type": "object"},
		},
		{
			Name:        "write_file",
			Description: "Write content to a file",
			InputSchema: map[string]any{"type": "object"},
		},
	})

	registry.AddServer("github", config.TransportHTTP, "GitHub repositories, issues and pull requests", true)
	registry.MarkConnected("github", []*mcp.Tool{
		{
			Name:        "search_issues",
			Description: "Search issues and pull requests",
		},
		{
			Name:        "create_pull_request",
			Description: "Open a pull request",
		},
	})

	registry.AddServer("broken", config.TransportStdio, "Unavailable server", true)
	registry.MarkError("broken", errors.New("connection refused"))

	return registry
}

func TestSearchServersRanksNameAboveDescription(t *testing.T) {
	registry := NewRegistry()
	registry.AddServer("tickets", config.TransportStdio, "Ticketing system", true)
	registry.MarkConnected("tickets", nil)
	registry.AddServer("jira", config.TransportHTTP, "Issue tickets and boards", true)
	registry.MarkConnected("jira", nil)

	got := registry.SearchServers("tickets", 5)
	if len(got) != 2 {
		t.Fatalf("SearchServers() returned %d servers, want 2", len(got))
	}
	if got[0].Name != "tickets" {
		t.Fatalf("SearchServers()[0].Name = %q, want tickets", got[0].Name)
	}
}

func TestSearchServersEmptyQuerySortedByLimit(t *testing.T) {
	registry := fixtureRegistry()

	all := registry.SearchServers("", 0)
	if len(all) != 3 {
		t.Fatalf("SearchServers(\"\") returned %d servers, want 3", len(all))
	}
	want := []string{"broken", "filesystem", "github"}
	for i, name := range want {
		if all[i].Name != name {
			t.Fatalf("SearchServers(\"\")[%d].Name = %q, want %q", i, all[i].Name, name)
		}
	}

	limited := registry.SearchServers("", 2)
	if len(limited) != 2 {
		t.Fatalf("SearchServers(\"\", 2) returned %d servers, want 2", len(limited))
	}
}

func TestSearchServersSkipsNonMatches(t *testing.T) {
	registry := fixtureRegistry()

	got := registry.SearchServers("filesystem", 5)
	if len(got) != 1 || got[0].Name != "filesystem" {
		t.Fatalf("SearchServers(%q) = %+v, want only filesystem", "filesystem", got)
	}
	if !got[0].Connected || got[0].ToolCount != 2 {
		t.Fatalf("filesystem info = %+v, want connected with 2 tools", got[0])
	}

	if none := registry.SearchServers("kubernetes", 5); len(none) != 0 {
		t.Fatalf("SearchServers(%q) = %+v, want no matches", "kubernetes", none)
	}
}

func TestSearchToolsAcrossServers(t *testing.T) {
	registry := fixtureRegistry()

	got := registry.SearchTools("pull request", "", 5)
	if len(got) < 2 {
		t.Fatalf("SearchTools() returned %d tools, want at least 2", len(got))
	}
	if got[0].Name != "create_pull_request" || got[0].Server != "github" {
		t.Fatalf("SearchTools()[0] = %+v, want github/create_pull_request", got[0])
	}
}

func TestSearchToolsServerFilter(t *testing.T) {
	registry := fixtureRegistry()

	got := registry.SearchTools("file", "filesystem", 5)
	if len(got) != 2 {
		t.Fatalf("SearchTools() returned %d tools, want 2", len(got))
	}
	for _, tool := range got {
		if tool.Server != "filesystem" {
			t.Fatalf("SearchTools() returned tool from %q, want only filesystem", tool.Server)
		}
	}
}

func TestSearchToolsExcludesDisconnectedServers(t *testing.T) {
	registry := fixtureRegistry()

	for _, tool := range registry.SearchTools("", "", 50) {
		if tool.Server == "broken" {
			t.Fatalf("SearchTools() returned %+v from a disconnected server", tool)
		}
	}
}

func TestToolDetailsExactMatch(t *testing.T) {
	registry := fixtureRegistry()

	got := registry.ToolDetails("", "read_file")
	if len(got) != 1 {
		t.Fatalf("ToolDetails() returned %d tools, want 1", len(got))
	}
	if got[0].Server != "filesystem" || got[0].Name != "read_file" {
		t.Fatalf("ToolDetails() = %+v, want filesystem/read_file", got[0])
	}
	if got[0].InputSchema == nil {
		t.Fatal("ToolDetails() lost the input schema")
	}
}

func TestToolDetailsSubstringFallback(t *testing.T) {
	registry := fixtureRegistry()

	got := registry.ToolDetails("", "FILE")
	if len(got) != 2 {
		t.Fatalf("ToolDetails() returned %d tools, want 2", len(got))
	}
}

func TestToolDetailsServerFilter(t *testing.T) {
	registry := fixtureRegistry()

	if got := registry.ToolDetails("github", "read_file"); len(got) != 0 {
		t.Fatalf("ToolDetails(github, read_file) = %+v, want no matches", got)
	}
}

func TestMarkErrorClearsTools(t *testing.T) {
	registry := NewRegistry()
	registry.AddServer("filesystem", config.TransportStdio, "Local files", true)
	registry.MarkConnected("filesystem", []*mcp.Tool{{Name: "read_file"}})
	registry.MarkError("filesystem", errors.New("gone"))

	if got := registry.SearchTools("", "", 5); len(got) != 0 {
		t.Fatalf("SearchTools() after MarkError returned %+v, want no tools", got)
	}
	servers := registry.Servers()
	if len(servers) != 1 || servers[0].Connected || servers[0].Error != "gone" {
		t.Fatalf("Servers() = %+v, want disconnected with error", servers)
	}
}
