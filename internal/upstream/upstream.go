// Package upstream connects to configured MCP servers and exposes their tools.
package upstream

import (
	"context"
	"errors"
	"fmt"
	"net/http"
	"os"
	"os/exec"
	"time"

	"github.com/modelcontextprotocol/go-sdk/mcp"
	"github.com/rLukoyanov/tokenmaxing/internal/config"
)

// defaultConnectTimeout bounds the MCP initialization handshake.
const defaultConnectTimeout = 20 * time.Second

// Conn is a live MCP session with a single upstream server.
type Conn struct {
	Name        string
	Description string
	Transport   config.Transport

	session *mcp.ClientSession
	cleanup func()
}

// Connect starts or dials the upstream server described by cfg and performs
// the MCP initialization handshake.
func Connect(ctx context.Context, version, name string, cfg config.Server) (*Conn, error) {
	kind, err := cfg.Transport()
	if err != nil {
		return nil, err
	}

	var (
		transport mcp.Transport
		cleanup   func()
	)

	switch kind {
	case config.TransportStdio:
		// The process must outlive the handshake, so it gets its own context
		// that is cancelled when the connection is closed.
		procCtx, cancel := context.WithCancel(context.Background())
		proc := exec.CommandContext(procCtx, cfg.Command, cfg.Args...)
		proc.Env = environWith(cfg.Env)
		transport = &mcp.CommandTransport{Command: proc}
		cleanup = cancel
	case config.TransportHTTP:
		transport = &mcp.StreamableClientTransport{
			Endpoint:   cfg.URL,
			HTTPClient: httpClientWithHeaders(cfg.Headers),
		}
	default:
		return nil, fmt.Errorf("unsupported transport %q", kind)
	}

	conn, err := connectWithTransport(ctx, version, name, cfg, transport, cleanup)
	if err != nil {
		return nil, err
	}
	return conn, nil
}

// connectWithTransport initializes a client session over transport. It is
// split out so tests can use in-memory transports.
func connectWithTransport(ctx context.Context, version, name string, cfg config.Server, transport mcp.Transport, cleanup func()) (*Conn, error) {
	kind, err := cfg.Transport()
	if err != nil {
		if cleanup != nil {
			cleanup()
		}
		return nil, err
	}

	client := mcp.NewClient(&mcp.Implementation{
		Name:    "tokenmaxing",
		Version: version,
	}, nil)

	connectCtx, cancel := context.WithTimeout(ctx, defaultConnectTimeout)
	defer cancel()

	session, err := client.Connect(connectCtx, transport, nil)
	if err != nil {
		if cleanup != nil {
			cleanup()
		}
		return nil, fmt.Errorf("initialize MCP session: %w", err)
	}

	return &Conn{
		Name:        name,
		Description: cfg.Description,
		Transport:   kind,
		session:     session,
		cleanup:     cleanup,
	}, nil
}

// Tools returns the tools advertised by the upstream server.
func (c *Conn) Tools(ctx context.Context) ([]*mcp.Tool, error) {
	var tools []*mcp.Tool
	for tool, err := range c.session.Tools(ctx, nil) {
		if err != nil {
			return nil, err
		}
		tools = append(tools, tool)
	}
	return tools, nil
}

// CallTool invokes a tool on the upstream server and returns its result
// unchanged, including structured content and the error flag.
func (c *Conn) CallTool(ctx context.Context, name string, arguments map[string]any) (*mcp.CallToolResult, error) {
	return c.session.CallTool(ctx, &mcp.CallToolParams{
		Name:      name,
		Arguments: arguments,
	})
}

// Close terminates the session and stops a locally spawned process.
func (c *Conn) Close() error {
	var errs []error
	if c.session != nil {
		errs = append(errs, c.session.Close())
	}
	if c.cleanup != nil {
		c.cleanup()
	}
	return errors.Join(errs...)
}

// environWith appends extra variables to the current process environment.
// Appended values win over inherited ones.
func environWith(extra map[string]string) []string {
	env := os.Environ()
	for k, v := range extra {
		env = append(env, k+"="+v)
	}
	return env
}

// httpClientWithHeaders returns an HTTP client that adds the configured
// headers to every request. It is used for authenticated remote servers.
func httpClientWithHeaders(headers map[string]string) *http.Client {
	base := http.DefaultTransport
	if http.DefaultClient.Transport != nil {
		base = http.DefaultClient.Transport
	}
	if len(headers) == 0 {
		return &http.Client{Transport: base}
	}
	return &http.Client{Transport: headerRoundTripper{base: base, headers: headers}}
}

type headerRoundTripper struct {
	base    http.RoundTripper
	headers map[string]string
}

func (h headerRoundTripper) RoundTrip(req *http.Request) (*http.Response, error) {
	clone := req.Clone(req.Context())
	for k, v := range h.headers {
		clone.Header.Set(k, v)
	}
	return h.base.RoundTrip(clone)
}
