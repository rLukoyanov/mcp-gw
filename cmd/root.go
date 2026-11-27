package cmd

import (
	"log/slog"
	"os"

	"github.com/spf13/cobra"
)

var (
	// cfgFile is the path to the configuration file, if provided.
	cfgFile string
	// verbose enables verbose logging.
	verbose bool
)

// rootCmd is the base command for the tokenmaxing CLI.
var rootCmd = &cobra.Command{
	Use:   "tokenmaxing",
	Short: "Proxy that reduces token consumption",
	Long: `tokenmaxing is a proxy that sits in front of upstream services
and reduces the number of tokens consumed by LLM clients.`,
	SilenceUsage:  true,
	SilenceErrors: true,
	PersistentPreRun: func(cmd *cobra.Command, args []string) {
		level := slog.LevelInfo
		if verbose {
			level = slog.LevelDebug
		}
		slog.SetDefault(slog.New(slog.NewTextHandler(os.Stderr, &slog.HandlerOptions{Level: level})))
	},
}

// Execute runs the root command and returns any execution error.
func Execute() error {
	return rootCmd.Execute()
}

func init() {
	rootCmd.PersistentFlags().StringVar(&cfgFile, "config", "", "path to the configuration file")
	rootCmd.PersistentFlags().BoolVarP(&verbose, "verbose", "v", false, "enable verbose logging")
}
