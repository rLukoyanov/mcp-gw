package gateway

import (
	"context"
	"os"
	"testing"
	"time"

	"github.com/modelcontextprotocol/go-sdk/mcp"
	"github.com/rLukoyanov/tokenmaxing/internal/config"
)

// TestUpstreamHelperServer runs only in the child process spawned by
// TestUpstreamsConnectAutoAndLazy, where it acts as a real stdio MCP server.
func TestUpstreamHelperServer(t *testing.T) {
	if os.Getenv("TOKENMAXING_TEST_UPSTREAM") != "1" {
		t.Skip("helper process only")
	}

	server := mcp.NewServer(&mcp.Implementation{Name: "helper", Version: "0.0.1"}, nil)
	mcp.AddTool(server, &mcp.Tool{Name: "echo", Description: "Echo text back"}, func(_ context.Context, _ *mcp.CallToolRequest, in struct {
		Text string `json:"text"`
	}) (*mcp.CallToolResult, any, error) {
		return &mcp.CallToolResult{
			Content: []mcp.Content{&mcp.TextContent{Text: "echo:" + in.Text}},
		}, nil, nil
	})

	if err := server.Run(context.Background(), &mcp.StdioTransport{}); err != nil {
		os.Exit(1)
	}
	os.Exit(0)
}

func TestUpstreamsConnectAutoAndLazy(t *testing.T) {
	ctx, cancel := context.WithTimeout(context.Background(), 60*time.Second)
	defer cancel()

	helper := config.Server{
		Command: os.Args[0],
		Args:    []string{"-test.run=TestUpstreamHelperServer"},
		Env:     map[string]string{"TOKENMAXING_TEST_UPSTREAM": "1"},
	}
	autoConnectOff := false
	lazy := helper
	lazy.Description = "Connected on demand"
	lazy.AutoConnect = &autoConnectOff

	upstreams := NewUpstreams("test", map[string]config.Server{
		"helper":  helper,
		"missing": {Command: "/nonexistent/tokenmaxing-test-server"},
		"lazy":    lazy,
	})
	t.Cleanup(func() { _ = upstreams.Close() })

	if got := upstreams.ConnectAuto(ctx); got != 1 {
		t.Fatalf("ConnectAuto() connected %d servers, want 1", got)
	}

	tools := upstreams.Registry().SearchTools("", "helper", 10)
	if len(tools) != 1 || tools[0].Name != "echo" {
		t.Fatalf("SearchTools(helper) = %+v, want the echo tool", tools)
	}

	infos := map[string]ServerInfo{}
	for _, info := range upstreams.Registry().Servers() {
		infos[info.Name] = info
	}
	if !infos["helper"].Connected {
		t.Fatal("helper is not connected")
	}
	if infos["missing"].Connected || infos["missing"].Error == "" {
		t.Fatalf("missing info = %+v, want disconnected with an error", infos["missing"])
	}
	if infos["lazy"].Connected || infos["lazy"].Error != "" {
		t.Fatalf("lazy info = %+v, want disconnected without an error", infos["lazy"])
	}
	if infos["lazy"].AutoConnect {
		t.Fatal("lazy server reports autoConnect=true")
	}

	first, err := upstreams.Ensure(ctx, "lazy")
	if err != nil {
		t.Fatalf("Ensure(lazy) error = %v", err)
	}
	tools = upstreams.Registry().SearchTools("", "lazy", 10)
	if len(tools) != 1 || tools[0].Name != "echo" {
		t.Fatalf("SearchTools(lazy) = %+v, want the echo tool after connecting", tools)
	}

	second, err := upstreams.Ensure(ctx, "lazy")
	if err != nil {
		t.Fatalf("second Ensure(lazy) error = %v", err)
	}
	if first != second {
		t.Fatal("Ensure() opened a second connection instead of reusing the first")
	}

	if _, err := upstreams.Ensure(ctx, "unknown"); err == nil {
		t.Fatal("Ensure(unknown) = nil, want error")
	}
}

func TestGatewayCallToolOverStdio(t *testing.T) {
	ctx, cancel := context.WithTimeout(context.Background(), 60*time.Second)
	defer cancel()

	upstreams := NewUpstreams("test", map[string]config.Server{
		"helper": {
			Command: os.Args[0],
			Args:    []string{"-test.run=TestUpstreamHelperServer"},
			Env:     map[string]string{"TOKENMAXING_TEST_UPSTREAM": "1"},
		},
	})
	t.Cleanup(func() { _ = upstreams.Close() })

	if got := upstreams.ConnectAuto(ctx); got != 1 {
		t.Fatalf("ConnectAuto() connected %d servers, want 1", got)
	}

	gw := New("test", upstreams.Registry(), upstreams)
	session := newTestClient(t, gw)

	result := callTool(t, session, "call_tool", map[string]any{
		"server":    "helper",
		"tool":      "echo",
		"arguments": map[string]any{"text": "hi"},
	})
	if result.IsError {
		t.Fatalf("call_tool failed: %+v", result.Content)
	}
	if len(result.Content) != 1 {
		t.Fatalf("call_tool returned %d content blocks, want 1", len(result.Content))
	}
	text, ok := result.Content[0].(*mcp.TextContent)
	if !ok || text.Text != "echo:hi" {
		t.Fatalf("call_tool content = %+v, want echo:hi", result.Content[0])
	}
}
