// The release this module was published with, and the SHA-256 of each platform's fastbrowse archive in it. A release
// writes the values onto the commit it tags go/vX.Y.Z; a checkout of the repository has none, and finds its binary
// through Options.BinaryPath or FASTBROWSE_BINARY.

package fastbrowse

var releaseVersion = "0.11.2"

var releaseChecksums = map[string]string{
	"darwin-arm64": "12232a72be5151badd1dfdad64f577c72c72ea5b2a44eab509dfb98124de93df",
	"darwin-x64":   "b339fd593ed3bd815296d42740e21341f31c92b5db7612aafaeab157cdcec636",
	"linux-arm64":  "0f532e652ccce775ceb4e89485e998d635fe49af2124d357cdfa2ac47288f067",
	"linux-x64":    "5c0db77ce3ae587c9ad0fccfd0c1e6a62a9b08ad3e33ec1360f4914da80f9c30",
	"win32-x64":    "b599e0b939484dfb82034b21481938facd4c4de4bb9ca231438c08c01e5d0824",
}
