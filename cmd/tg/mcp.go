package main

import (
	"context"
	"net/netip"
	"os"
	"os/signal"
	"syscall"

	"github.com/xiaotianxt/tg/internal/env"
	"github.com/xiaotianxt/tg/internal/mcp"
	"github.com/xiaotianxt/tg/internal/mcp/config"
	"github.com/xiaotianxt/tg/internal/mcp/rpc"
	"github.com/xiaotianxt/tg/internal/mcp/tools"
	"github.com/xiaotianxt/tg/internal/tgtime"
)

// mcpRegistry builds the tool registry over the source flags.
func mcpRegistry(e env.Env, src *sourceFlags) mcp.BuildRegistry {
	return func(policy config.RemotePolicy, remote bool) (*rpc.Registry, error) {
		return tools.NewRegistry(tools.Context{
			Env: e, DBDir: src.dbDir, Keys: src.keys, Jobs: int(src.jobs), Policy: policy, Remote: remote,
		})
	}
}

func signalContext() (context.Context, context.CancelFunc) {
	return signal.NotifyContext(context.Background(), os.Interrupt, syscall.SIGTERM)
}

func runMCPStdio(e env.Env, src *sourceFlags, remotePolicy bool) error {
	ctx, stop := signalContext()
	defer stop()
	return mcp.Stdio(ctx, e, mcp.StdioOptions{RemotePolicy: remotePolicy, Version: version, Build: mcpRegistry(e, src)}, os.Stdin, os.Stdout)
}

// runMCPInit converts the already syntax-checked address flags.
func runMCPInit(e env.Env, publicURL string, listen, trustedProxy *addrValue, redirectURIs []string, passphraseStdin bool) error {
	o := mcp.InitOptions{PublicURL: publicURL, RedirectURIs: redirectURIs, PassphraseStdin: passphraseStdin}
	if listen.value != "" {
		addr, err := netip.ParseAddrPort(listen.value)
		if err != nil {
			return err
		}
		o.Listen = addr
	}
	if trustedProxy.value != "" {
		addr, err := netip.ParseAddr(trustedProxy.value)
		if err != nil {
			return err
		}
		o.TrustedProxy = addr
	}
	return mcp.Init(e, o, os.Stdin, os.Stdout)
}

func runMCPServe(e env.Env, src *sourceFlags) error {
	ctx, stop := signalContext()
	defer stop()
	probe := func() error {
		st, err := src.open()
		if err != nil {
			return err
		}
		return st.Close()
	}
	return mcp.Serve(ctx, e, mcp.ServeOptions{Version: version, Build: mcpRegistry(e, src), Probe: probe})
}

func runMCPAudit(e env.Env, since string) error {
	ts, err := tgtime.Parse(since, e.Now(), e.Loc)
	if err != nil {
		return err
	}
	return mcp.Audit(e, mcp.AuditOptions{Since: ts}, os.Stdout)
}
