package fastbrowse

import (
	"archive/tar"
	"bytes"
	"compress/gzip"
	"context"
	"crypto/sha256"
	"encoding/hex"
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"strings"
	"testing"
)

// release serves archive as this platform's archive of release 1.2.3, pinned to checksum, for one test.
func release(t *testing.T, archive []byte, checksum string) (cache string, requests *int) {
	t.Helper()
	platform, err := target()
	if err != nil {
		t.Skip(err)
	}
	requests = new(int)
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		*requests++
		if r.URL.Path != "/v1.2.3/fastbrowse-rust-"+platform+".tar.gz" {
			http.NotFound(w, r)
			return
		}
		_, _ = w.Write(archive)
	}))
	t.Cleanup(server.Close)
	savedURL, savedVersion, savedChecksums := releases, releaseVersion, releaseChecksums
	releases, releaseVersion, releaseChecksums = server.URL, "1.2.3", map[string]string{platform: checksum}
	t.Cleanup(func() { releases, releaseVersion, releaseChecksums = savedURL, savedVersion, savedChecksums })
	return t.TempDir(), requests
}

// archiveOf is a release archive as the release workflow packs one: the binary under fastbrowse/.
func archiveOf(t *testing.T, content string) ([]byte, string) {
	t.Helper()
	var buffer bytes.Buffer
	zipped := gzip.NewWriter(&buffer)
	entries := tar.NewWriter(zipped)
	for name, body := range map[string]string{"fastbrowse/README": "x", "fastbrowse/" + executable(): content} {
		_ = entries.WriteHeader(&tar.Header{Name: name, Mode: 0o644, Size: int64(len(body))})
		_, _ = entries.Write([]byte(body))
	}
	_ = entries.Close()
	_ = zipped.Close()
	digest := sha256.Sum256(buffer.Bytes())
	return buffer.Bytes(), hex.EncodeToString(digest[:])
}

func TestTheReleaseBinaryIsDownloadedOnceAndRunnable(t *testing.T) {
	archive, checksum := archiveOf(t, "#!/bin/sh\n")
	cache, requests := release(t, archive, checksum)
	binary, err := releaseBinary(context.Background(), cache)
	if err != nil {
		t.Fatal(err)
	}
	content, _ := os.ReadFile(binary)
	info, _ := os.Stat(binary)
	if string(content) != "#!/bin/sh\n" || info.Mode()&0o100 == 0 {
		t.Fatalf("got %q, mode %v", content, info.Mode())
	}
	if again, err := releaseBinary(context.Background(), cache); err != nil || again != binary || *requests != 1 {
		t.Fatalf("second resolve: %s, %v after %d downloads", again, err, *requests)
	}
}

func TestAnArchiveThatIsNotThePinnedOneIsRefusedAndNothingIsKept(t *testing.T) {
	archive, _ := archiveOf(t, "tampered")
	cache, _ := release(t, archive, strings.Repeat("0", 64))
	_, err := releaseBinary(context.Background(), cache)
	if err == nil || !strings.Contains(err.Error(), "this module pins") {
		t.Fatalf("err: %v", err)
	}
	kept, _ := filepath.Glob(filepath.Join(cache, "*", "*", "*"))
	if len(kept) != 0 {
		t.Fatalf("left behind: %v", kept)
	}
}

func TestACheckoutThatIsNotAReleaseNamesTheWayToABinary(t *testing.T) {
	saved := releaseVersion
	releaseVersion = ""
	t.Cleanup(func() { releaseVersion = saved })
	t.Setenv("FASTBROWSE_BINARY", "")
	_, err := resolveBinary(context.Background(), "")
	if err == nil || !strings.Contains(err.Error(), "FASTBROWSE_BINARY") {
		t.Fatalf("err: %v", err)
	}
}

func TestTheCallersPathComesFirstAndThenTheEnvironments(t *testing.T) {
	t.Setenv("FASTBROWSE_BINARY", "/from/env")
	if got, _ := resolveBinary(context.Background(), "/mine"); got != "/mine" {
		t.Fatalf("got %s", got)
	}
	if got, _ := resolveBinary(context.Background(), ""); got != "/from/env" {
		t.Fatalf("got %s", got)
	}
}
