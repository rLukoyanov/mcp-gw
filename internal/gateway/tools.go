package gateway

import (
	"context"
	"errors"
	"fmt"

	"github.com/modelcontextprotocol/go-sdk/mcp"
)

// Gateway exposes discovery tools over MCP. Upstream tools are not listed
// directly; they are found through search and invoked through call_tool.
type Gateway struct {
	registry  *Registry
	upstreams UpstreamProvider
	server    *mcp.Server
}

// New builds a gateway with the discovery and call tools registered. The
// upstreams provider may be nil, in which case only already-indexed data is
// searchable and call_tool fails.
func New(version string, registry *Registry, upstreams UpstreamProvider) *Gateway {
	if registry == nil {
		registry = NewRegistry()
	}

	server := mcp.NewServer(&mcp.Implementation{
		Name:        "tokenmaxing",
		Title:       "tokenmaxing",
		Description: "Proxy that reduces token consumption by exposing upstream MCP tools on demand",
		Version:     version,
	}, &mcp.ServerOptions{
		Instructions: "Upstream tools are not listed directly. Use search_servers to find a server by capability, " +
			"search_tools to find tools across connected servers, get_tool_details to read a tool's input schema, " +
			"and call_tool to invoke it. Servers with autoConnect disabled are connected when you first target them.",
	})

	g := &Gateway{registry: registry, upstreams: upstreams, server: server}

	mcp.AddTool(server, &mcp.Tool{
		Name: "search_servers",
		Description: "Search the upstream MCP servers configured for this proxy by name and description. " +
			"Returns compact summaries. Start here when you need a capability and do not know which server provides it. " +
			"Servers with connected=false can still be used: target them by name to connect on demand.",
		Annotations: readOnlyAnnotations(),
	}, g.searchServers)

	mcp.AddTool(server, &mcp.Tool{
		Name: "search_tools",
		Description: "Search tools across the upstream MCP servers by name and description. " +
			"Optionally restrict the search to one server; naming a disconnected server connects it first. " +
			"Returns compact summaries; use get_tool_details for the full input schema.",
		Annotations: readOnlyAnnotations(),
	}, g.searchTools)

	mcp.AddTool(server, &mcp.Tool{
		Name: "get_tool_details",
		Description: "Get the full description and input schema of a tool on an upstream server. " +
			"Pass an exact tool name, or a substring when the exact name is unknown. " +
			"Naming a disconnected server connects it first.",
		Annotations: readOnlyAnnotations(),
	}, g.getToolDetails)

	mcp.AddTool(server, &mcp.Tool{
		Name: "call_tool",
		Description: "Invoke a tool on an upstream MCP server and return its result unchanged, including structured content. " +
			"A disconnected server (autoConnect=false) is connected on demand. Use search_tools and get_tool_details first.",
	}, g.callTool)

	return g
}

// Server returns the MCP server exposing the discovery tools.
func (g *Gateway) Server() *mcp.Server {
	return g.server
}

// Run serves the gateway over the given transport until ctx is cancelled.
func (g *Gateway) Run(ctx context.Context, transport mcp.Transport) error {
	return g.server.Run(ctx, transport)
}

// ensure connects a named upstream if it is not connected yet.
func (g *Gateway) ensure(ctx context.Context, server string) error {
	info, ok := g.registry.Server(server)
	if !ok {
		return fmt.Errorf("unknown upstream server %q", server)
	}
	if info.Connected {
		return nil
	}
	if g.upstreams == nil {
		return errors.New("no upstream connections configured")
	}
	_, err := g.upstreams.Ensure(ctx, server)
	return err
}

// SearchServersInput is the input schema of search_servers.
type SearchServersInput struct {
	Query string `json:"query" jsonschema:"capability you are looking for, in natural language"`
	Limit int    `json:"limit,omitempty" jsonschema:"maximum number of results to return (default 5, max 50)"`
}

// SearchServersOutput is the output schema of search_servers.
type SearchServersOutput struct {
	Servers []ServerInfo `json:"servers"`
	Message string       `json:"message,omitempty"`
}

func (g *Gateway) searchServers(_ context.Context, _ *mcp.CallToolRequest, input SearchServersInput) (*mcp.CallToolResult, SearchServersOutput, error) {
	servers := g.registry.SearchServers(input.Query, input.Limit)
	if len(servers) == 0 {
		return nil, SearchServersOutput{
			Servers: []ServerInfo{},
			Message: "no upstream servers matched; try broader terms or an empty query to list all servers",
		}, nil
	}
	return nil, SearchServersOutput{Servers: servers}, nil
}

// SearchToolsInput is the input schema of search_tools.
type SearchToolsInput struct {
	Query  string `json:"query" jsonschema:"what the tool should do, in natural language"`
	Server string `json:"server,omitempty" jsonschema:"restrict the search to this upstream server name, connecting it if needed"`
	Limit  int    `json:"limit,omitempty" jsonschema:"maximum number of results to return (default 5, max 50)"`
}

// SearchToolsOutput is the output schema of search_tools.
type SearchToolsOutput struct {
	Tools   []ToolMatch `json:"tools"`
	Message string      `json:"message,omitempty"`
}

func (g *Gateway) searchTools(ctx context.Context, _ *mcp.CallToolRequest, input SearchToolsInput) (*mcp.CallToolResult, SearchToolsOutput, error) {
	if input.Server != "" {
		if err := g.ensure(ctx, input.Server); err != nil {
			return nil, SearchToolsOutput{}, fmt.Errorf("upstream server %q: %w", input.Server, err)
		}
	}

	tools := g.registry.SearchTools(input.Query, input.Server, input.Limit)
	if len(tools) == 0 {
		return nil, SearchToolsOutput{
			Tools:   []ToolMatch{},
			Message: "no tools matched; name an on-demand server to connect it, or use search_servers to check the server name",
		}, nil
	}
	return nil, SearchToolsOutput{Tools: tools}, nil
}

// GetToolDetailsInput is the input schema of get_tool_details.
type GetToolDetailsInput struct {
	Tool   string `json:"tool" jsonschema:"tool name, or a substring if the exact name is unknown"`
	Server string `json:"server,omitempty" jsonschema:"restrict the lookup to this upstream server name, connecting it if needed"`
}

// GetToolDetailsOutput is the output schema of get_tool_details.
type GetToolDetailsOutput struct {
	Tools   []ToolDetail `json:"tools"`
	Message string       `json:"message,omitempty"`
}

func (g *Gateway) getToolDetails(ctx context.Context, _ *mcp.CallToolRequest, input GetToolDetailsInput) (*mcp.CallToolResult, GetToolDetailsOutput, error) {
	if input.Server != "" {
		if err := g.ensure(ctx, input.Server); err != nil {
			return nil, GetToolDetailsOutput{}, fmt.Errorf("upstream server %q: %w", input.Server, err)
		}
	}

	tools := g.registry.ToolDetails(input.Server, input.Tool)
	if len(tools) == 0 {
		return nil, GetToolDetailsOutput{
			Tools:   []ToolDetail{},
			Message: "no tool matched; use search_tools to find the exact tool name",
		}, nil
	}
	return nil, GetToolDetailsOutput{Tools: tools}, nil
}

// CallToolInput is the input schema of call_tool.
type CallToolInput struct {
	Server    string         `json:"server" jsonschema:"upstream server name from search_servers or search_tools"`
	Tool      string         `json:"tool" jsonschema:"exact tool name from search_tools or get_tool_details"`
	Arguments map[string]any `json:"arguments,omitempty" jsonschema:"arguments matching the tool input schema"`
}

// callTool invokes an upstream tool and relays its result unchanged.
func (g *Gateway) callTool(ctx context.Context, _ *mcp.CallToolRequest, input CallToolInput) (*mcp.CallToolResult, any, error) {
	if g.upstreams == nil {
		return nil, nil, errors.New("no upstream connections configured")
	}

	caller, err := g.upstreams.Ensure(ctx, input.Server)
	if err != nil {
		return nil, nil, fmt.Errorf("upstream server %q: %w", input.Server, err)
	}

	result, err := caller.CallTool(ctx, input.Tool, input.Arguments)
	if err != nil {
		return nil, nil, fmt.Errorf("call %q on %q: %w", input.Tool, input.Server, err)
	}
	return result, nil, nil
}

// readOnlyAnnotations marks the discovery tools as side-effect free.
func readOnlyAnnotations() *mcp.ToolAnnotations {
	destructive := false
	openWorld := false
	return &mcp.ToolAnnotations{
		ReadOnlyHint:    true,
		IdempotentHint:  true,
		DestructiveHint: &destructive,
		OpenWorldHint:   &openWorld,
	}
}
