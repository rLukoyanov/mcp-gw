package gateway

import (
	"context"
	"encoding/json"
	"fmt"
	"strings"
	"sync"
	"testing"

	"github.com/modelcontextprotocol/go-sdk/mcp"
	tiktoken "github.com/pkoukk/tiktoken-go"
	tiktokenloader "github.com/pkoukk/tiktoken-go-loader"
	"github.com/rLukoyanov/tokenmaxing/internal/config"
)

// These tests estimate how many tokens a client spends on tool definitions
// and on a complete tool-calling round trip, with the gateway and without it.
// Counts use the o200k_base tokenizer and a synthetic, deterministic catalog;
// they are an approximation of real model token usage.

var (
	encoderOnce sync.Once
	encoder     *tiktoken.Tiktoken
	encoderErr  error
)

func tokenEncoder(t *testing.T) *tiktoken.Tiktoken {
	t.Helper()
	encoderOnce.Do(func() {
		tiktoken.SetBpeLoader(tiktokenloader.NewOfflineLoader())
		encoder, encoderErr = tiktoken.GetEncoding(tiktoken.MODEL_O200K_BASE)
	})
	if encoderErr != nil {
		t.Fatalf("load tokenizer: %v", encoderErr)
	}
	return encoder
}

func countTokens(enc *tiktoken.Tiktoken, text string) int {
	if text == "" {
		return 0
	}
	return len(enc.Encode(text, nil, nil))
}

// benchServer is one synthetic upstream server in the catalog.
type benchServer struct {
	name        string
	description string
	tools       []*mcp.Tool
}

var benchDomains = []struct{ name, description string }{
	{"filesystem", "Read, write and search files on the local filesystem"},
	{"github", "GitHub repositories, issues, pull requests and code search"},
	{"slack", "Slack channels, messages and the user directory"},
	{"postgres", "Query PostgreSQL databases and inspect their schemas"},
	{"browser", "Navigate web pages, click elements and capture screenshots"},
}

var benchVerbs = []string{"read", "write", "search", "list", "create", "update", "delete", "get"}

var benchObjects = []string{"file", "issue", "channel", "table", "page", "record", "document", "repository"}

var benchProperties = []string{"path", "query", "limit", "content", "owner", "repo", "channel", "text", "sql", "url", "id", "filter"}

// buildCatalog generates a deterministic catalog of servers and tools.
func buildCatalog(servers, toolsPerServer int) []benchServer {
	catalog := make([]benchServer, 0, servers)
	for i := 0; i < servers; i++ {
		domain := benchDomains[i%len(benchDomains)]
		name := domain.name
		if round := i / len(benchDomains); round > 0 {
			name = fmt.Sprintf("%s-%d", domain.name, round+1)
		}

		used := make(map[string]struct{}, toolsPerServer)
		server := benchServer{name: name, description: domain.description}
		for j := 0; j < toolsPerServer; j++ {
			verb := benchVerbs[(i+j)%len(benchVerbs)]
			object := benchObjects[(i*3+j)%len(benchObjects)]
			toolName := verb + "_" + object
			if _, exists := used[toolName]; exists {
				toolName = fmt.Sprintf("%s_%d", toolName, j)
			}
			used[toolName] = struct{}{}

			server.tools = append(server.tools, &mcp.Tool{
				Name: toolName,
				Description: fmt.Sprintf(
					"Use this tool to %s a %s on the %s server. Pass the required parameters and the server returns the matching %s data.",
					verb, object, domain.name, object),
				InputSchema: benchSchema(i, j),
			})
		}
		catalog = append(catalog, server)
	}
	return catalog
}

func benchSchema(serverIndex, toolIndex int) map[string]any {
	properties := make(map[string]any)
	var required []string
	propertyCount := 2 + (serverIndex+toolIndex)%4

	for offset := 0; len(properties) < propertyCount; offset++ {
		name := benchProperties[(serverIndex*2+toolIndex*3+offset)%len(benchProperties)]
		if _, exists := properties[name]; exists {
			continue
		}
		properties[name] = map[string]any{
			"type":        "string",
			"description": fmt.Sprintf("The %s parameter for this operation.", name),
		}
		if len(required) < 2 {
			required = append(required, name)
		}
	}

	schema := map[string]any{"type": "object", "properties": properties}
	if len(required) > 0 {
		schema["required"] = required
	}
	return schema
}

func catalogTools(catalog []benchServer) []*mcp.Tool {
	var tools []*mcp.Tool
	for _, server := range catalog {
		tools = append(tools, server.tools...)
	}
	return tools
}

func renderToolDefs(t *testing.T, tools []*mcp.Tool) string {
	t.Helper()
	raw, err := json.Marshal(tools)
	if err != nil {
		t.Fatalf("marshal tool definitions: %v", err)
	}
	return string(raw)
}

func registryFromCatalog(catalog []benchServer) *Registry {
	registry := NewRegistry()
	for _, server := range catalog {
		registry.AddServer(server.name, config.TransportStdio, server.description, true)
		registry.MarkConnected(server.name, server.tools)
	}
	return registry
}

// gatewayToolDefs returns the tools the gateway actually advertises.
func gatewayToolDefs(t *testing.T, session *mcp.ClientSession) []*mcp.Tool {
	t.Helper()
	var tools []*mcp.Tool
	for tool, err := range session.Tools(context.Background(), nil) {
		if err != nil {
			t.Fatalf("list gateway tools: %v", err)
		}
		tools = append(tools, tool)
	}
	return tools
}

// toolResultText renders a tool result the way a client sees it.
func toolResultText(t *testing.T, result *mcp.CallToolResult) string {
	t.Helper()
	var b strings.Builder
	for _, content := range result.Content {
		if text, ok := content.(*mcp.TextContent); ok {
			b.WriteString(text.Text)
		}
	}
	if b.Len() == 0 && result.StructuredContent != nil {
		raw, err := json.Marshal(result.StructuredContent)
		if err != nil {
			t.Fatalf("marshal structured content: %v", err)
		}
		b.Write(raw)
	}
	return b.String()
}

// benchToolResult is the payload returned by the target tool. It is identical
// in both scenarios, so it does not affect the comparison.
const benchToolResult = "Quarterly report: revenue grew 12% year over year, gross margin improved " +
	"to 71%, and operating expenses stayed flat. The strongest regions were EMEA and APAC; " +
	"churn dropped to 1.8%. Risks: hiring delays in the platform team and one large renewal " +
	"moving to the next quarter. Recommended next steps: approve two additional headcount, " +
	"accelerate the renewal conversation, and review cloud spend for the staging environment."

const benchUserRequest = "Please read the file /tmp/report.txt and summarize the key numbers."

// directFlowTokens estimates input tokens for a client that sees every
// upstream tool: two requests (tool call, then final answer).
func directFlowTokens(t *testing.T, enc *tiktoken.Tiktoken, catalog []benchServer) (defs, total int) {
	t.Helper()
	target := catalog[len(catalog)/2]
	targetTool := target.tools[len(target.tools)/2]

	defsText := renderToolDefs(t, catalogTools(catalog))
	toolCall := fmt.Sprintf(`{"name":%q,"arguments":{"path":"/tmp/report.txt"}}`, targetTool.Name)

	first := defsText + "\n" + benchUserRequest
	second := first + "\n" + toolCall + "\n" + benchToolResult

	return countTokens(enc, defsText), countTokens(enc, first) + countTokens(enc, second)
}

// gatewayFlowTokens estimates input tokens for the same task through the
// gateway: discovery, schema lookup, then the tool call.
func gatewayFlowTokens(t *testing.T, enc *tiktoken.Tiktoken, catalog []benchServer, session *mcp.ClientSession) (defs, total int) {
	t.Helper()

	target := catalog[len(catalog)/2]
	targetTool := target.tools[len(target.tools)/2]

	defsText := renderToolDefs(t, gatewayToolDefs(t, session))
	history := defsText + "\n" + benchUserRequest
	total = countTokens(enc, history)

	steps := []struct {
		call   string
		result *mcp.CallToolResult
	}{
		{
			call: fmt.Sprintf(`{"name":"search_servers","arguments":{"query":%q,"limit":3}}`, target.description),
			// result below
		},
		{
			call: fmt.Sprintf(`{"name":"search_tools","arguments":{"query":%q,"server":%q,"limit":3}}`,
				targetTool.Description, target.name),
		},
		{
			call: fmt.Sprintf(`{"name":"get_tool_details","arguments":{"tool":%q,"server":%q}}`,
				targetTool.Name, target.name),
		},
	}

	requests := []map[string]any{
		{"query": target.description, "limit": 3},
		{"query": targetTool.Description, "server": target.name, "limit": 3},
		{"tool": targetTool.Name, "server": target.name},
	}
	names := []string{"search_servers", "search_tools", "get_tool_details"}

	for i, name := range names {
		result, err := session.CallTool(context.Background(), &mcp.CallToolParams{
			Name:      name,
			Arguments: requests[i],
		})
		if err != nil {
			t.Fatalf("call %s: %v", name, err)
		}
		steps[i].result = result
	}

	for _, step := range steps {
		history += "\n" + step.call + "\n" + toolResultText(t, step.result)
		total += countTokens(enc, history)
	}

	finalCall := fmt.Sprintf(`{"name":"call_tool","arguments":{"server":%q,"tool":%q,"arguments":{"path":"/tmp/report.txt"}}}`, target.name, targetTool.Name)
	history += "\n" + finalCall + "\n" + benchToolResult
	total += countTokens(enc, history)

	return countTokens(enc, defsText), total
}

type tokenScenario struct {
	servers int
	tools   int
}

func TestTokenSpendWithAndWithoutGateway(t *testing.T) {
	enc := tokenEncoder(t)

	scenarios := []tokenScenario{
		{1, 1},
		{1, 4},
		{2, 4},
		{3, 4},
		{5, 8},
		{10, 10},
	}

	t.Logf("%-8s %12s %12s %10s %12s %12s %10s",
		"catalog", "defs direct", "defs gw", "defs save", "flow direct", "flow gw", "flow save")

	for _, scenario := range scenarios {
		name := fmt.Sprintf("%dx%d", scenario.servers, scenario.tools)
		catalog := buildCatalog(scenario.servers, scenario.tools)
		registry := registryFromCatalog(catalog)
		gw := New("test", registry, nil)
		session := newTestClient(t, gw)

		directDefs, directFlow := directFlowTokens(t, enc, catalog)
		gatewayDefs, gatewayFlow := gatewayFlowTokens(t, enc, catalog, session)

		defsSaving := 100 * (1 - float64(gatewayDefs)/float64(directDefs))
		flowSaving := 100 * (1 - float64(gatewayFlow)/float64(directFlow))

		t.Logf("%-8s %12d %12d %9.1f%% %12d %12d %9.1f%%",
			name, directDefs, gatewayDefs, defsSaving, directFlow, gatewayFlow, flowSaving)

		if gatewayDefs <= 0 || gatewayFlow <= 0 {
			t.Fatalf("%s: token counts must be positive", name)
		}
	}
}

func TestGatewayReducesToolDefinitionTokens(t *testing.T) {
	enc := tokenEncoder(t)
	catalog := buildCatalog(5, 8)
	gw := New("test", registryFromCatalog(catalog), nil)
	session := newTestClient(t, gw)

	directDefs := countTokens(enc, renderToolDefs(t, catalogTools(catalog)))
	gatewayDefs := countTokens(enc, renderToolDefs(t, gatewayToolDefs(t, session)))

	if gatewayDefs >= directDefs {
		t.Fatalf("gateway tool definitions cost %d tokens, direct catalog %d; gateway should be cheaper",
			gatewayDefs, directDefs)
	}
}

func TestGatewayReducesFlowTokensForLargeCatalog(t *testing.T) {
	enc := tokenEncoder(t)
	catalog := buildCatalog(5, 8)
	gw := New("test", registryFromCatalog(catalog), nil)
	session := newTestClient(t, gw)

	_, directFlow := directFlowTokens(t, enc, catalog)
	_, gatewayFlow := gatewayFlowTokens(t, enc, catalog, session)

	if gatewayFlow >= directFlow {
		t.Fatalf("gateway flow costs %d tokens, direct flow %d; gateway should be cheaper for 40 tools",
			gatewayFlow, directFlow)
	}
}

func TestGatewayCostsMoreForTinyCatalog(t *testing.T) {
	enc := tokenEncoder(t)
	catalog := buildCatalog(1, 1)
	gw := New("test", registryFromCatalog(catalog), nil)
	session := newTestClient(t, gw)

	_, directFlow := directFlowTokens(t, enc, catalog)
	_, gatewayFlow := gatewayFlowTokens(t, enc, catalog, session)

	if gatewayFlow <= directFlow {
		t.Fatalf("gateway flow costs %d tokens, direct flow %d; discovery overhead should dominate for 1 tool",
			gatewayFlow, directFlow)
	}
}
