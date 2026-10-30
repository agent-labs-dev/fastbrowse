// The release this module was published with, and the SHA-256 of each platform's fastbrowse archive in it. A release
// writes the values onto the commit it tags go/vX.Y.Z; a checkout of the repository has none, and finds its binary
// through Options.BinaryPath or FASTBROWSE_BINARY.

package fastbrowse

var releaseVersion = "0.9.1"

var releaseChecksums = map[string]string{
	"darwin-arm64": "b76c9aad7c99331f880610816a8d39e3de1f3aa1dd12a5498d17cea84aa31457",
	"darwin-x64":   "e8e57f7bafd9e9b7de2ab184be058c330886dd25abb7280cd75e1d83f42ec42c",
	"linux-arm64":  "1ca1d9c98ca69a476fbc2be9ce648b7195418013be9dfe84ff0bb8013747493d",
	"linux-x64":    "933a06ad843ae7b5c833483f6eb975fd497aaaf17ad534fae1401631533c53be",
	"win32-x64":    "e2fdab2b65c3752f04c5c8ce9e29bcd924e5dd9d88da0ea0f2349e60513c900e",
}
