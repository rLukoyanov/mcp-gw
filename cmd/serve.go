package cmd

import (
	"context"
	"errors"
	"fmt"
	"log/slog"
	"os"
	"os/signal"
	"syscall"

	"github.com/modelcontextprotocol/go-sdk/mcp"
	"github.com/rLukoyanov/tokenmaxing/internal/config"
	"github.com/rLukoyanov/tokenmaxing/internal/gateway"
	"github.com/spf13/cobra"
)

var serveCmd = &cobra.Command{
	Use:   "serve",
	Short: "Start the token-reducing proxy",
	Long: `Start the token-reducing proxy. It connects to the configured upstream
MCP servers, indexes their tools and serves discovery and call tools over stdio.
Servers with "autoConnect": false are connected on demand.`,
	RunE: func(cmd *cobra.Command, args []string) error {
		path, err := config.ResolvePath(cfgFile)
		if err != nil {
			return err
		}
		cfg, err := config.Load(path)
		if err != nil {
			return err
		}
		if cfg.Empty() {
			return fmt.Errorf("no MCP servers configured in %s", path)
		}

		parent := cmd.Context()
		if parent == nil {
			parent = context.Background()
		}
		ctx, stop := signal.NotifyContext(parent, os.Interrupt, syscall.SIGTERM)
		defer stop()

		upstreams := gateway.NewUpstreams(version, cfg.Servers)
		defer func() {
			if err := upstreams.Close(); err != nil {
				slog.Warn("close upstream servers", "error", err)
			}
		}()

		connected := upstreams.ConnectAuto(ctx)
		if connected == 0 {
			return errors.New("no upstream MCP servers could be connected")
		}

		slog.Info("tokenmaxing gateway ready",
			"config", path,
			"configured", len(cfg.Servers),
			"connected", connected,
		)

		return gateway.New(version, upstreams.Registry(), upstreams).Run(ctx, &mcp.StdioTransport{})
	},
}

func init() {
	rootCmd.AddCommand(serveCmd)
}
