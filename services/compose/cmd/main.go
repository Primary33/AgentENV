package main

import (
	"context"
	"encoding/json"
	"fmt"
	"io"
	"os"

	"agentenv/services/compose"
)

func main() {
	var request compose.Request
	decoder := json.NewDecoder(io.LimitReader(os.Stdin, 2*1024*1024))
	decoder.DisallowUnknownFields()
	if err := decoder.Decode(&request); err != nil {
		fail(err)
	}
	var extra any
	if err := decoder.Decode(&extra); err != io.EOF {
		fail(fmt.Errorf("expected one JSON request"))
	}
	plan, err := compose.Prepare(context.Background(), request)
	if err != nil {
		fail(err)
	}
	if err := json.NewEncoder(os.Stdout).Encode(plan); err != nil {
		fmt.Fprintln(os.Stderr, err)
		os.Exit(1)
	}
}

func fail(err error) {
	fmt.Fprintln(os.Stderr, err)
	os.Exit(2)
}
