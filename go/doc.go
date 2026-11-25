// Package fastbrowse runs browser tasks with fastbrowse, a browser agent that picks from the controls a page actually
// has: every page is indexed into the controls it has, one LLM conversation names the actions to take, and code owns
// authorization, secrets and cost limits, and reports an answer it cannot verify as unverified.
//
// A Client starts the fastbrowse binary as `fastbrowse serve --stdio` and speaks its JSON-RPC protocol. The binary is
// the one the module's release pins, downloaded once and checked against its SHA-256, unless Options.BinaryPath or
// FASTBROWSE_BINARY names another. Model and browser keys are read from the process environment, as the fastbrowse
// command reads them.
//
//	client, err := fastbrowse.Start(ctx, fastbrowse.Options{})
//	if err != nil {
//		return err
//	}
//	defer client.Close(context.Background())
//	result, err := client.Run(ctx, "What is the top story's title?", fastbrowse.RunOptions{
//		Start:  "https://news.ycombinator.com/",
//		Limits: &fastbrowse.Limits{MaxDollars: ptr(0.25)},
//	})
//
// A run ends in a Status: a run that needs a login or got stuck is read from the result, not returned as an error.
// One Client runs one task at a time; start several for runs in parallel.
package fastbrowse

//go:generate go run ./internal/gentypes ../protocol/schema.json protocol.go
