package cmd

import (
	"fmt"
	"runtime"

	"github.com/spf13/cobra"
)

// Build metadata. Override at link time with:
//
//	go build -ldflags "-X github.com/rLukoyanov/tokenmaxing/cmd.version=1.0.0 ..."
var (
	version = "dev"
	commit  = "none"
	date    = "unknown"
)

var versionCmd = &cobra.Command{
	Use:   "version",
	Short: "Print version information",
	Run: func(cmd *cobra.Command, args []string) {
		fmt.Fprintf(cmd.OutOrStdout(), "tokenmaxing %s (commit %s, built %s, %s)\n",
			version, commit, date, runtime.Version())
	},
}

func init() {
	rootCmd.AddCommand(versionCmd)
}
