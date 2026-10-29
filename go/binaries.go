// The release this module was published with, and the SHA-256 of each platform's fastbrowse archive in it. A release
// writes the values onto the commit it tags go/vX.Y.Z; a checkout of the repository has none, and finds its binary
// through Options.BinaryPath or FASTBROWSE_BINARY.

package fastbrowse

var releaseVersion = "0.9.0"

var releaseChecksums = map[string]string{
	"darwin-arm64": "52f2a2e95ed3b003de91a535e5435edb73bb5f796d2c902219a514ceacfb68d5",
	"darwin-x64":   "1edc0054afb0ac5802bc92b122499f0780420d6545704c9dba2c89be7c6ad801",
	"linux-arm64":  "83021c7ba5cb56ef49c98374ef5d5f60ec450aa72e759319b0dce6d13cbcbf18",
	"linux-x64":    "5d1864b0c536b05489fa8209a0a33f1ce6a0c1b450900a360f4ebca0c43cd1ed",
	"win32-x64":    "cfb84a96742516a7a692bcd1e2d1b2513c794d15c0a1efa576032674832951dc",
}
