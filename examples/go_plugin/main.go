// A minimal out-of-process zuno plugin in Go, standard library only.
//
// Build it and drop the binary in a scanned directory:
//
//	go build -o ~/.zuno/plugin/go-example ./examples/go_plugin
//
// Discovery needs no manifest and no extension: on Unix the executable bit is the
// signal, so the built binary is a plugin the moment it lands in `plugin/`.
//
// Standard output carries newline-delimited JSON-RPC and nothing else. Every
// diagnostic goes to standard error, because one stray line on stdout is an
// unparseable frame and the host permanently disables a plugin that sends one.
package main

import (
	"bufio"
	"encoding/json"
	"fmt"
	"os"
)

const protocolVersion = "1.0"

type request struct {
	JSONRPC string          `json:"jsonrpc"`
	ID      json.RawMessage `json:"id"`
	Method  string          `json:"method"`
	Params  json.RawMessage `json:"params"`
}

type response struct {
	JSONRPC string          `json:"jsonrpc"`
	ID      json.RawMessage `json:"id"`
	Result  any             `json:"result,omitempty"`
	Error   *rpcError       `json:"error,omitempty"`
}

type rpcError struct {
	Code    int    `json:"code"`
	Message string `json:"message"`
}

type toolDefinition struct {
	ID          string `json:"id"`
	Description string `json:"description"`
	Parameters  any    `json:"parameters"`
}

type manifest struct {
	ID    string           `json:"id"`
	Hooks []string         `json:"hooks"`
	Tools []toolDefinition `json:"tools"`
}

type initializeResult struct {
	ProtocolVersion string   `json:"protocolVersion"`
	Plugin          manifest `json:"plugin"`
}

type initializeParams struct {
	ProtocolVersions []string `json:"protocolVersions"`
}

type hookCall struct {
	Hook   string         `json:"hook"`
	Input  map[string]any `json:"input"`
	Output map[string]any `json:"output"`
}

type toolCall struct {
	Tool      string         `json:"tool"`
	Arguments map[string]any `json:"arguments"`
}

func pluginManifest() manifest {
	return manifest{
		// `tool` must be declared for the tool below to be accepted: the host rejects
		// a plugin that returns tools without naming the resource.
		ID:    "go-example",
		Hooks: []string{"tool", "shell.env"},
		Tools: []toolDefinition{{
			ID:          "go_echo",
			Description: "Echo text from a Go plugin",
			Parameters: map[string]any{
				"type":       "object",
				"properties": map[string]any{"text": map[string]any{"type": "string"}},
				"required":   []string{"text"},
			},
		}},
	}
}

func initialize(params json.RawMessage) (any, *rpcError) {
	var decoded initializeParams
	if err := json.Unmarshal(params, &decoded); err != nil {
		return nil, &rpcError{Code: -32602, Message: fmt.Sprintf("invalid params: %v", err)}
	}
	for _, offered := range decoded.ProtocolVersions {
		if offered == protocolVersion {
			return initializeResult{ProtocolVersion: protocolVersion, Plugin: pluginManifest()}, nil
		}
	}
	return nil, &rpcError{
		Code:    -32001,
		Message: fmt.Sprintf("plugin supports protocol %s", protocolVersion),
	}
}

func callHook(params json.RawMessage) (any, *rpcError) {
	var call hookCall
	if err := json.Unmarshal(params, &call); err != nil {
		return nil, &rpcError{Code: -32602, Message: fmt.Sprintf("invalid params: %v", err)}
	}
	if call.Output == nil {
		call.Output = map[string]any{}
	}
	if call.Hook == "shell.env" {
		env, ok := call.Output["env"].(map[string]any)
		if !ok {
			env = map[string]any{}
		}
		env["GO_PLUGIN"] = "enabled"
		call.Output["env"] = env
	}
	return map[string]any{"output": call.Output}, nil
}

func callTool(params json.RawMessage) (any, *rpcError) {
	var call toolCall
	if err := json.Unmarshal(params, &call); err != nil {
		return nil, &rpcError{Code: -32602, Message: fmt.Sprintf("invalid params: %v", err)}
	}
	if call.Tool != "go_echo" {
		return nil, &rpcError{Code: -32010, Message: fmt.Sprintf("tool %q is not registered", call.Tool)}
	}
	text, ok := call.Arguments["text"].(string)
	if !ok {
		return nil, &rpcError{Code: -32010, Message: "text must be a string"}
	}
	return map[string]any{"title": "Go echo", "output": text}, nil
}

func main() {
	reader := bufio.NewScanner(os.Stdin)
	// A hook payload can carry a whole transcript, so the default 64KiB token limit is
	// not enough: a truncated line is a malformed frame and a permanent disable.
	reader.Buffer(make([]byte, 0, 64*1024), 16*1024*1024)
	writer := bufio.NewWriter(os.Stdout)
	initialized := false
	for reader.Scan() {
		line := reader.Bytes()
		if len(line) == 0 {
			continue
		}
		var incoming request
		if err := json.Unmarshal(line, &incoming); err != nil {
			fmt.Fprintf(os.Stderr, "go-example: malformed frame: %v\n", err)
			continue
		}
		var result any
		var failure *rpcError
		switch {
		case incoming.Method == "plugin.initialize":
			result, failure = initialize(incoming.Params)
			if failure == nil {
				initialized = true
			}
		case !initialized:
			failure = &rpcError{Code: -32002, Message: "plugin is not initialized"}
		case incoming.Method == "hook.call":
			result, failure = callHook(incoming.Params)
		case incoming.Method == "tool.call":
			result, failure = callTool(incoming.Params)
		default:
			failure = &rpcError{Code: -32601, Message: "method not found"}
		}
		encoded, err := json.Marshal(response{
			JSONRPC: "2.0",
			ID:      incoming.ID,
			Result:  result,
			Error:   failure,
		})
		if err != nil {
			fmt.Fprintf(os.Stderr, "go-example: could not encode response: %v\n", err)
			continue
		}
		writer.Write(encoded)
		writer.WriteByte('\n')
		if err := writer.Flush(); err != nil {
			fmt.Fprintf(os.Stderr, "go-example: could not flush: %v\n", err)
			return
		}
	}
	if err := reader.Err(); err != nil {
		fmt.Fprintf(os.Stderr, "go-example: stdin failed: %v\n", err)
	}
}
