// Command gentypes writes protocol.go, the client's types for everything on the wire, from protocol/schema.json.
//
//	go generate ./...
//
// Each model under `$defs` becomes a struct, each enum a named type with a constant per value, and each entry
// under `constants` (the protocol version, the error codes and the method names) a constant, so the client holds
// no copy of one that could fall behind the server's.
package main

import (
	"bytes"
	"encoding/json"
	"fmt"
	"go/format"
	"os"
	"sort"
	"strings"
)

type schema struct {
	Type                 any                `json:"type"`
	Description          string             `json:"description"`
	Ref                  string             `json:"$ref"`
	AnyOf                []*schema          `json:"anyOf"`
	Enum                 []any              `json:"enum"`
	Format               string             `json:"format"`
	Properties           orderedProperties  `json:"properties"`
	Required             []string           `json:"required"`
	Items                *schema            `json:"items"`
	PrefixItems          []*schema          `json:"prefixItems"`
	AdditionalProperties json.RawMessage    `json:"additionalProperties"`
	Defs                 map[string]*schema `json:"$defs"`
}

type property struct {
	Name   string
	Schema *schema
}

// orderedProperties keeps the properties in the order the schema writes them, which is the order of the fields on
// the wire.
type orderedProperties []property

func (p *orderedProperties) UnmarshalJSON(data []byte) error {
	decoder := json.NewDecoder(bytes.NewReader(data))
	if _, err := decoder.Token(); err != nil {
		return err
	}
	for decoder.More() {
		key, err := decoder.Token()
		if err != nil {
			return err
		}
		var value schema
		if err := decoder.Decode(&value); err != nil {
			return err
		}
		*p = append(*p, property{Name: key.(string), Schema: &value})
	}
	_, err := decoder.Token()
	return err
}

type document struct {
	Defs      map[string]*schema         `json:"$defs"`
	Constants map[string]json.RawMessage `json:"constants"`
}

// Words Go spells in capitals.
var initialisms = map[string]string{
	"id": "ID", "url": "URL", "uri": "URI", "cdp": "CDP", "llm": "LLM", "api": "API", "json": "JSON", "rpc": "RPC",
	"sha256": "SHA256",
}

func goName(name string) string {
	var out strings.Builder
	for _, word := range strings.FieldsFunc(name, func(r rune) bool { return r == '_' || r == '/' || r == '-' }) {
		lower := strings.ToLower(word)
		if initialism, ok := initialisms[lower]; ok {
			out.WriteString(initialism)
		} else {
			out.WriteString(strings.ToUpper(word[:1]) + word[1:])
		}
	}
	return out.String()
}

// Definitions with no Go shape of their own: what they hold is passed through as written.
var raw = map[string]bool{"JsonValue": true, "RequestId": true}

type generator struct {
	out bytes.Buffer
}

func (g *generator) printf(format string, args ...any) {
	fmt.Fprintf(&g.out, format, args...)
}

func (g *generator) comment(text, indent string) {
	for _, line := range strings.Split(strings.TrimSpace(text), "\n") {
		g.printf("%s// %s\n", indent, strings.TrimRight(line, " "))
	}
}

func isNull(s *schema) bool { return s.Type == "null" }

// goType is the Go type a property's schema reads into. A type that can be absent without a pointer (a slice, a
// map, raw JSON) stays as it is when the schema also allows null.
func (g *generator) goType(s *schema) string {
	if s.Ref != "" {
		name := strings.TrimPrefix(s.Ref, "#/$defs/")
		if raw[name] {
			return "json.RawMessage"
		}
		return name
	}
	if len(s.AnyOf) > 0 {
		var others []*schema
		for _, branch := range s.AnyOf {
			if !isNull(branch) {
				others = append(others, branch)
			}
		}
		if len(others) != 1 {
			return "json.RawMessage"
		}
		inner := g.goType(others[0])
		if len(others) == len(s.AnyOf) || strings.HasPrefix(inner, "[]") || strings.HasPrefix(inner, "map[") ||
			inner == "json.RawMessage" {
			return inner
		}
		return "*" + inner
	}
	switch s.Type {
	case "string":
		if s.Format == "base64" {
			return "[]byte"
		}
		return "string"
	case "integer":
		return "int64"
	case "number":
		return "float64"
	case "boolean":
		return "bool"
	case "array":
		if len(s.PrefixItems) > 0 {
			return fmt.Sprintf("[%d]%s", len(s.PrefixItems), g.goType(s.PrefixItems[0]))
		}
		return "[]" + g.goType(s.Items)
	case "object":
		var additional schema
		if json.Unmarshal(s.AdditionalProperties, &additional) == nil && additional.Type != nil {
			return "map[string]" + g.goType(&additional)
		}
		return "json.RawMessage"
	}
	return "json.RawMessage"
}

func (g *generator) model(name string, s *schema) {
	if s.Description != "" {
		g.comment(s.Description, "")
	}
	g.printf("type %s struct {\n", name)
	required := map[string]bool{}
	for _, key := range s.Required {
		required[key] = true
	}
	for _, p := range s.Properties {
		if p.Schema.Description != "" {
			g.comment(p.Schema.Description, "\t")
		}
		tag := p.Name
		if !required[p.Name] {
			tag += ",omitempty"
		}
		g.printf("\t%s %s `json:%q`\n", goName(p.Name), g.goType(p.Schema), tag)
	}
	g.printf("}\n\n")
}

func (g *generator) enum(name string, s *schema, names map[string]string) {
	if s.Description != "" {
		g.comment(s.Description, "")
	}
	base := "string"
	if s.Type == "integer" {
		base = "int64"
	}
	g.printf("type %s %s\n\nconst (\n", name, base)
	for _, value := range s.Enum {
		switch v := value.(type) {
		case string:
			g.printf("\t%s%s %s = %q\n", name, goName(v), name, v)
		case float64:
			g.printf("\t%s%s %s = %d\n", name, names[fmt.Sprint(int64(v))], name, int64(v))
		}
	}
	g.printf(")\n\n")
}

func main() {
	if len(os.Args) != 3 {
		fmt.Fprintln(os.Stderr, "usage: gentypes SCHEMA OUTPUT")
		os.Exit(2)
	}
	data, err := os.ReadFile(os.Args[1])
	if err != nil {
		panic(err)
	}
	var doc document
	if err := json.Unmarshal(data, &doc); err != nil {
		panic(err)
	}
	g := &generator{}
	g.printf("// Code generated by internal/gentypes from protocol/schema.json, which crates/fastbrowse-core/src/protocol.rs\n")
	g.printf("// writes. DO NOT EDIT: change the types there, then run `go generate ./...`.\n\n")
	g.printf("package fastbrowse\n\nimport \"encoding/json\"\n\n")

	var version int64
	if err := json.Unmarshal(doc.Constants["PROTOCOL_VERSION"], &version); err != nil {
		panic(err)
	}
	g.printf("// ProtocolVersion is the version of the wire format this client speaks.\nconst ProtocolVersion = %d\n\n", version)

	// The methods a client calls have no definition of their own, only their names.
	var methods map[string]string
	if err := json.Unmarshal(doc.Constants["Method"], &methods); err != nil {
		panic(err)
	}
	g.printf("// What a client can ask of the server.\nconst (\n")
	for _, member := range sortedKeys(methods) {
		g.printf("\tMethod%s = %q\n", goName(strings.ToLower(member)), methods[member])
	}
	g.printf(")\n\n")

	// The error codes are numbers, and their names come from the constants.
	var codes map[string]int64
	if err := json.Unmarshal(doc.Constants["ErrorCode"], &codes); err != nil {
		panic(err)
	}
	codeNames := map[string]string{}
	for member, code := range codes {
		codeNames[fmt.Sprint(code)] = goName(strings.ToLower(member))
	}

	for _, name := range sortedKeys(doc.Defs) {
		s := doc.Defs[name]
		switch {
		case raw[name]:
		case len(s.Enum) > 0:
			g.enum(name, s, codeNames)
		case s.Type == "object":
			g.model(name, s)
		}
	}
	source, err := format.Source(g.out.Bytes())
	if err != nil {
		os.Stderr.Write(g.out.Bytes())
		panic(err)
	}
	if err := os.WriteFile(os.Args[2], source, 0o644); err != nil {
		panic(err)
	}
}

func sortedKeys[V any](m map[string]V) []string {
	keys := make([]string, 0, len(m))
	for key := range m {
		keys = append(keys, key)
	}
	sort.Strings(keys)
	return keys
}
