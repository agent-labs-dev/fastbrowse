// The release this module was published with, and the SHA-256 of each platform's fastbrowse archive in it. A release
// writes the values onto the commit it tags go/vX.Y.Z; a checkout of the repository has none, and finds its binary
// through Options.BinaryPath or FASTBROWSE_BINARY.

package fastbrowse

var releaseVersion = "0.12.0"

var releaseChecksums = map[string]string{
	"darwin-arm64": "67138909f997521358c766cb178c0c7fcc8c9aab63ab8b0528f0ec81387bdb1a",
	"darwin-x64":   "9cc7e6a347c175ee0598d0d5843a27d18bbbcbb7080b12ecbb037401597056cb",
	"linux-arm64":  "12a66c71abcc433a53ab68c95a499d9cf9e01ab78e63a4c024d7c0e46b58b440",
	"linux-x64":    "fbfe17e2cf46603fb7f52dc9efe3b3848febc9ada43a7b529d11ea743fc6db4a",
	"win32-x64":    "2e5ad78f65395c51ac40361a8221069f0e1926fdf5e1842baa9746af9c06a04d",
}
