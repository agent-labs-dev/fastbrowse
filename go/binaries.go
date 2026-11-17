// The release this module was published with, and the SHA-256 of each platform's fastbrowse archive in it. A release
// writes the values onto the commit it tags go/vX.Y.Z; a checkout of the repository has none, and finds its binary
// through Options.BinaryPath or FASTBROWSE_BINARY.

package fastbrowse

var releaseVersion = "0.11.1"

var releaseChecksums = map[string]string{
	"darwin-arm64": "158f56d436cd105494f037caaec7774aba0afab4953cf9e00c64678b23feb22f",
	"darwin-x64":   "3ca244bf4cfdf9d5c514aa7d5a26f2d05f48ccd27fd3370aee11689745a88bcc",
	"linux-arm64":  "7939903e53d8231551be99b4628fd45e21798e9b6f857fb151197281398166d3",
	"linux-x64":    "f7d8a67c52a6387685a264eefd0b5b54fad9ce372efa4805f603caa38ff3039f",
	"win32-x64":    "d58e7531b2ec5a69750fc5d36445ad420318eac0a9d5981da7b58ad2a1a52e5d",
}
