package gateway

import (
	"context"
	"encoding/json"
	"errors"
	"slices"
	"strings"
	"testing"

	"github.com/modelcontextprotocol/go-sdk/mcp"
	"github.com/rLukoyanov/tokenmaxing/internal/config"
)

func newTestClient(t *testing.T, gw *Gateway) *mcp.ClientSession {
	t.Helper()
	ctx := context.Background()

	serverTransport, clientTransport := mcp.NewInMemoryTransports()
	if _, err := gw.Server().Connect(ctx, serverTransport, nil); err != nil {
		t.Fatalf("connect gateway server: %v", err)
	}

	client := mcp.NewClient(&mcp.Implementation{Name: "test-client", Version: "0.0.1"}, nil)
	session, err := client.Connect(ctx, clientTransport, nil)
	if err != nil {
		t.Fatalf("connect test client: %v", err)
	}
	t.Cleanup(func() { _ = session.Close() })
	return session
}

func structured[T any](t *testing.T, result *mcp.CallToolResult) T {
	t.Helper()
	if result.IsError {
		t.Fatalf("tool call failed: %+v", result.Content)
	}
	raw, err := json.Marshal(result.StructuredContent)
	if err != nil {
		t.Fatalf("marshal structured content: %v", err)
	}
	var out T
	if err := json.Unmarshal(raw, &out); err != nil {
		t.Fatalf("unmarshal structured content: %v", err)
	}
	return out
}

func callTool(t *testing.T, session *mcp.ClientSession, name string, args map[string]any) *mcp.CallToolResult {
	t.Helper()
	result, err := session.CallTool(context.Background(), &mcp.CallToolParams{Name: name, Arguments: args})
	if err != nil {
		t.Fatalf("CallTool(%s) error = %v", name, err)
	}
	return result
}

func TestGatewayAdvertisesOnlyDiscoveryTools(t *testing.T) {
	gw := New("test", fixtureRegistry(), nil)
	session := newTestClient(t, gw)

	var names []string
	for tool, err := range session.Tools(context.Background(), nil) {
		if err != nil {
			t.Fatalf("Tools() error = %v", err)
		}
		names = append(names, tool.Name)
	}

	want := []string{"call_tool", "get_tool_details", "search_servers", "search_tools"}
	slices.Sort(names)
	if !slices.Equal(names, want) {
		t.Fatalf("advertised tools = %v, want %v", names, want)
	}
}

func TestGatewaySearchServers(t *testing.T) {
	gw := New("test", fixtureRegistry(), nil)
	session := newTestClient(t, gw)

	out := structured[SearchServersOutput](t, callTool(t, session, "search_servers", map[string]any{
		"query": "filesystem",
	}))
	if len(out.Servers) != 1 || out.Servers[0].Name != "filesystem" {
		t.Fatalf("search_servers returned %+v, want only filesystem", out.Servers)
	}
	if !out.Servers[0].Connected || out.Servers[0].ToolCount != 2 {
		t.Fatalf("filesystem summary = %+v, want connected with 2 tools", out.Servers[0])
	}
}

func TestGatewaySearchTools(t *testing.T) {
	gw := New("test", fixtureRegistry(), nil)
	session := newTestClient(t, gw)

	out := structured[SearchToolsOutput](t, callTool(t, session, "search_tools", map[string]any{
		"query":  "pull request",
		"server": "github",
	}))
	if len(out.Tools) == 0 {
		t.Fatal("search_tools returned no tools")
	}
	if out.Tools[0].Name != "create_pull_request" || out.Tools[0].Server != "github" {
		t.Fatalf("search_tools[0] = %+v, want github/create_pull_request", out.Tools[0])
	}
}

func TestGatewayGetToolDetails(t *testing.T) {
	gw := New("test", fixtureRegistry(), nil)
	session := newTestClient(t, gw)

	out := structured[GetToolDetailsOutput](t, callTool(t, session, "get_tool_details", map[string]any{
		"tool": "read_file",
	}))
	if len(out.Tools) != 1 {
		t.Fatalf("get_tool_details returned %d tools, want 1", len(out.Tools))
	}
	if out.Tools[0].InputSchema == nil {
		t.Fatal("get_tool_details lost the input schema")
	}
}

func TestGatewayGetToolDetailsMissingTool(t *testing.T) {
	gw := New("test", fixtureRegistry(), nil)
	session := newTestClient(t, gw)

	out := structured[GetToolDetailsOutput](t, callTool(t, session, "get_tool_details", map[string]any{
		"tool": "nonexistent",
	}))
	if len(out.Tools) != 0 {
		t.Fatalf("get_tool_details returned %+v, want no tools", out.Tools)
	}
	if out.Message == "" {
		t.Fatal("get_tool_details returned no hint for a missing tool")
	}
}

// fakeCaller records the call and returns a canned result.
type fakeCaller struct {
	gotTool string
	gotArgs map[string]any
	result  *mcp.CallToolResult
	err     error
}

func (f *fakeCaller) CallTool(_ context.Context, name string, arguments map[string]any) (*mcp.CallToolResult, error) {
	f.gotTool = name
	f.gotArgs = arguments
	return f.result, f.err
}

// fakeProvider records which server the gateway asked to connect.
type fakeProvider struct {
	ensured  string
	caller   ToolCaller
	err      error
	onEnsure func(name string)
}

func (f *fakeProvider) Ensure(_ context.Context, name string) (ToolCaller, error) {
	f.ensured = name
	if f.onEnsure != nil {
		f.onEnsure(name)
	}
	return f.caller, f.err
}

func TestGatewayCallTool(t *testing.T) {
	caller := &fakeCaller{result: &mcp.CallToolResult{
		Content: []mcp.Content{&mcp.TextContent{Text: "upstream result"}},
	}}
	provider := &fakeProvider{caller: caller}
	gw := New("test", fixtureRegistry(), provider)
	session := newTestClient(t, gw)

	result := callTool(t, session, "call_tool", map[string]any{
		"server":    "github",
		"tool":      "create_pull_request",
		"arguments": map[string]any{"title": "Fix bug"},
	})

	if provider.ensured != "github" {
		t.Fatalf("Ensure() called with %q, want github", provider.ensured)
	}
	if caller.gotTool != "create_pull_request" {
		t.Fatalf("CallTool() tool = %q, want create_pull_request", caller.gotTool)
	}
	if caller.gotArgs["title"] != "Fix bug" {
		t.Fatalf("CallTool() arguments = %+v, want title=Fix bug", caller.gotArgs)
	}
	if len(result.Content) != 1 {
		t.Fatalf("call_tool returned %d content blocks, want 1", len(result.Content))
	}
	text, ok := result.Content[0].(*mcp.TextContent)
	if !ok || text.Text != "upstream result" {
		t.Fatalf("call_tool content = %+v, want the upstream text", result.Content[0])
	}
}

func TestGatewayCallToolReportsConnectError(t *testing.T) {
	provider := &fakeProvider{err: errors.New("connection refused")}
	gw := New("test", fixtureRegistry(), provider)
	session := newTestClient(t, gw)

	result := callTool(t, session, "call_tool", map[string]any{
		"server": "github",
		"tool":   "create_pull_request",
	})
	if !result.IsError {
		t.Fatal("call_tool did not report an error")
	}
	if len(result.Content) != 1 {
		t.Fatalf("call_tool returned %d content blocks, want 1", len(result.Content))
	}
	text, ok := result.Content[0].(*mcp.TextContent)
	if !ok || !strings.Contains(text.Text, "connection refused") {
		t.Fatalf("call_tool error = %+v, want it to mention connection refused", result.Content[0])
	}
}

func TestGatewaySearchToolsConnectsOnDemand(t *testing.T) {
	registry := fixtureRegistry()
	registry.AddServer("ondemand", config.TransportStdio, "Lazily loaded server", false)

	provider := &fakeProvider{
		caller: &fakeCaller{},
		onEnsure: func(name string) {
			registry.MarkConnected(name, []*mcp.Tool{{
				Name:        "lazy_tool",
				Description: "Only available after connecting",
			}})
		},
	}
	gw := New("test", registry, provider)
	session := newTestClient(t, gw)

	out := structured[SearchToolsOutput](t, callTool(t, session, "search_tools", map[string]any{
		"query":  "",
		"server": "ondemand",
	}))
	if provider.ensured != "ondemand" {
		t.Fatalf("Ensure() called with %q, want ondemand", provider.ensured)
	}
	if len(out.Tools) != 1 || out.Tools[0].Name != "lazy_tool" {
		t.Fatalf("search_tools returned %+v, want lazy_tool", out.Tools)
	}
}
