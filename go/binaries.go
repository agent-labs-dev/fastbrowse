// The release this module was published with, and the SHA-256 of each platform's fastbrowse archive in it. A release
// writes the values onto the commit it tags go/vX.Y.Z; a checkout of the repository has none, and finds its binary
// through Options.BinaryPath or FASTBROWSE_BINARY.

package fastbrowse

var releaseVersion = "0.12.1"

var releaseChecksums = map[string]string{
	"darwin-arm64": "639fc79e0ba25222a826b106c7aa1f0fb6e3236c558a9029958763378c6fe16f",
	"darwin-x64":   "aac5e14c4eedcd800b81c7b38112b29ff336f20ac730e8a7be389111d84c6457",
	"linux-arm64":  "cc742b335d0b79e0b7ce75b6f9f984dc09298793ffd87060c30210a86a438597",
	"linux-x64":    "a2cc3d0f11f0e7c04fdcbf6861b82adb527072ff0083c68fd8052fc7d811368b",
	"win32-x64":    "f5981281246e8270591c455e9ee6189cd9dee16407324e0289956a0030d996c5",
}
