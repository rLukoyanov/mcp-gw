package upstream

import (
	"context"
	"net/http"
	"net/http/httptest"
	"os"
	"slices"
	"testing"

	"github.com/modelcontextprotocol/go-sdk/mcp"
	"github.com/rLukoyanov/tokenmaxing/internal/config"
)

func TestConnTools(t *testing.T) {
	ctx := context.Background()

	server := mcp.NewServer(&mcp.Implementation{Name: "fake", Version: "0.0.1"}, nil)
	mcp.AddTool(server, &mcp.Tool{Name: "echo", Description: "Echo text back"}, func(_ context.Context, _ *mcp.CallToolRequest, in struct {
		Text string `json:"text"`
	}) (*mcp.CallToolResult, any, error) {
		return nil, nil, nil
	})

	serverTransport, clientTransport := mcp.NewInMemoryTransports()
	if _, err := server.Connect(ctx, serverTransport, nil); err != nil {
		t.Fatalf("connect fake server: %v", err)
	}

	client := mcp.NewClient(&mcp.Implementation{Name: "test", Version: "0.0.1"}, nil)
	session, err := client.Connect(ctx, clientTransport, nil)
	if err != nil {
		t.Fatalf("connect client: %v", err)
	}
	t.Cleanup(func() { _ = session.Close() })

	conn := &Conn{Name: "fake", Transport: config.TransportStdio, session: session}
	tools, err := conn.Tools(ctx)
	if err != nil {
		t.Fatalf("Tools() error = %v", err)
	}
	if len(tools) != 1 {
		t.Fatalf("Tools() returned %d tools, want 1", len(tools))
	}
	if tools[0].Name != "echo" {
		t.Fatalf("Tools()[0].Name = %q, want echo", tools[0].Name)
	}
}

func TestConnectRejectsInvalidConfig(t *testing.T) {
	if _, err := Connect(context.Background(), "0.0.1", "bad", config.Server{}); err == nil {
		t.Fatal("Connect() = nil, want error for server without command or url")
	}
}

func TestHTTPClientWithHeaders(t *testing.T) {
	var got http.Header
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		got = r.Header.Clone()
		w.WriteHeader(http.StatusNoContent)
	}))
	t.Cleanup(server.Close)

	resp, err := httpClientWithHeaders(map[string]string{
		"Authorization": "Bearer secret",
		"X-Test":        "1",
	}).Get(server.URL)
	if err != nil {
		t.Fatalf("GET error = %v", err)
	}
	t.Cleanup(func() { _ = resp.Body.Close() })

	if got.Get("Authorization") != "Bearer secret" {
		t.Fatalf("Authorization header = %q, want %q", got.Get("Authorization"), "Bearer secret")
	}
	if got.Get("X-Test") != "1" {
		t.Fatalf("X-Test header = %q, want 1", got.Get("X-Test"))
	}
}

func TestEnvironWith(t *testing.T) {
	env := environWith(map[string]string{"TOKENMAXING_TEST_ENV": "value"})

	if !slices.Contains(env, "TOKENMAXING_TEST_ENV=value") {
		t.Fatalf("environWith() does not contain the extra variable: %v", env)
	}
	if len(env) <= len(os.Environ()) {
		t.Fatalf("environWith() returned %d entries, want more than %d", len(env), len(os.Environ()))
	}
}
