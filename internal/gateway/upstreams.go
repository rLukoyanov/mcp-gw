package gateway

import (
	"context"
	"errors"
	"fmt"
	"log/slog"
	"sort"
	"sync"
	"time"

	"github.com/modelcontextprotocol/go-sdk/mcp"
	"github.com/rLukoyanov/tokenmaxing/internal/config"
	"github.com/rLukoyanov/tokenmaxing/internal/upstream"
)

// listToolsTimeout bounds loading the tool list from a single server.
const listToolsTimeout = 30 * time.Second

// ToolCaller forwards a tool call to one upstream server.
type ToolCaller interface {
	CallTool(ctx context.Context, name string, arguments map[string]any) (*mcp.CallToolResult, error)
}

// UpstreamProvider returns a connection to a named upstream server,
// establishing it on demand when the server is not connected yet.
type UpstreamProvider interface {
	Ensure(ctx context.Context, name string) (ToolCaller, error)
}

// Upstreams owns the connections to configured MCP servers and the index of
// the tools they advertise.
type Upstreams struct {
	registry *Registry
	version  string
	servers  map[string]config.Server

	mu    sync.Mutex
	conns map[string]*upstream.Conn
	locks map[string]*sync.Mutex
}

// NewUpstreams records every configured server in the registry. Servers with
// autoConnect enabled are connected later by ConnectAuto.
func NewUpstreams(version string, servers map[string]config.Server) *Upstreams {
	u := &Upstreams{
		registry: NewRegistry(),
		version:  version,
		servers:  servers,
		conns:    make(map[string]*upstream.Conn),
		locks:    make(map[string]*sync.Mutex),
	}

	for name, server := range servers {
		autoConnect := server.AutoConnectEnabled()
		transport, err := server.Transport()
		if err != nil {
			u.registry.AddServer(name, "", server.Description, autoConnect)
			u.registry.MarkError(name, err)
			continue
		}
		u.registry.AddServer(name, transport, server.Description, autoConnect)
	}

	return u
}

// Registry exposes the tool index to the discovery tools.
func (u *Upstreams) Registry() *Registry {
	return u.registry
}

// ConnectAuto connects every server with autoConnect enabled, concurrently,
// and returns the number of servers that connected successfully.
func (u *Upstreams) ConnectAuto(ctx context.Context) int {
	var names []string
	for name, server := range u.servers {
		if server.AutoConnectEnabled() {
			names = append(names, name)
		}
	}
	sort.Strings(names)

	var wg sync.WaitGroup
	for _, name := range names {
		wg.Add(1)
		go func(name string) {
			defer wg.Done()
			if _, err := u.Ensure(ctx, name); err != nil {
				slog.Warn("upstream server unavailable", "server", name, "error", err)
			}
		}(name)
	}
	wg.Wait()

	return u.connectedCount()
}

// Ensure returns a live connection to the named server, connecting and
// indexing its tools on first use. Concurrent callers share one connection.
func (u *Upstreams) Ensure(ctx context.Context, name string) (ToolCaller, error) {
	server, ok := u.servers[name]
	if !ok {
		return nil, fmt.Errorf("unknown upstream server %q", name)
	}

	lock := u.lockFor(name)
	lock.Lock()
	defer lock.Unlock()

	u.mu.Lock()
	conn := u.conns[name]
	u.mu.Unlock()
	if conn != nil {
		return conn, nil
	}

	conn, err := upstream.Connect(ctx, u.version, name, server)
	if err != nil {
		u.registry.MarkError(name, err)
		return nil, err
	}

	listCtx, cancel := context.WithTimeout(ctx, listToolsTimeout)
	defer cancel()
	tools, err := conn.Tools(listCtx)
	if err != nil {
		_ = conn.Close()
		err = fmt.Errorf("list tools: %w", err)
		u.registry.MarkError(name, err)
		return nil, err
	}

	u.mu.Lock()
	u.conns[name] = conn
	u.mu.Unlock()
	u.registry.MarkConnected(name, tools)
	slog.Info("connected upstream server", "server", name, "tools", len(tools))

	return conn, nil
}

// Close terminates every established connection.
func (u *Upstreams) Close() error {
	u.mu.Lock()
	defer u.mu.Unlock()

	var errs []error
	for name, conn := range u.conns {
		if err := conn.Close(); err != nil {
			errs = append(errs, fmt.Errorf("close %q: %w", name, err))
		}
		delete(u.conns, name)
	}
	return errors.Join(errs...)
}

func (u *Upstreams) connectedCount() int {
	count := 0
	for _, server := range u.registry.Servers() {
		if server.Connected {
			count++
		}
	}
	return count
}

// lockFor returns the per-server mutex serializing connection attempts.
func (u *Upstreams) lockFor(name string) *sync.Mutex {
	u.mu.Lock()
	defer u.mu.Unlock()

	lock, ok := u.locks[name]
	if !ok {
		lock = &sync.Mutex{}
		u.locks[name] = lock
	}
	return lock
}
