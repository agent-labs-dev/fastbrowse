// The release this module was published with, and the SHA-256 of each platform's fastbrowse archive in it. A release
// writes the values onto the commit it tags go/vX.Y.Z; a checkout of the repository has none, and finds its binary
// through Options.BinaryPath or FASTBROWSE_BINARY.

package fastbrowse

var releaseVersion = "0.10.0"

var releaseChecksums = map[string]string{
	"darwin-arm64": "acdb3fc7378c2a5dfc181f0cbc350ca118253972452eff9ed9ce08cadb1ecb07",
	"darwin-x64":   "aaa4efc682366cf3b0a5a0fd55d71cd2db9f0a14e1a37d4862bbeaea37da40e8",
	"linux-arm64":  "f23b32bcce1e61901911a0c6cf5f8871b211e7d6ffa6f78010a9731c1475fa33",
	"linux-x64":    "5de47a92b735c990cd6474964017df1fb73d9ec3e5f72699333500af14ec4998",
	"win32-x64":    "f0d08483d72e4fad521751d184728e1254914e1c59d5b4194b38c1a11f289cde",
}
