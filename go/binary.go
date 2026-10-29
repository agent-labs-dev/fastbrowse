package fastbrowse

import (
	"archive/tar"
	"compress/gzip"
	"context"
	"crypto/sha256"
	"encoding/hex"
	"errors"
	"fmt"
	"io"
	"net/http"
	"os"
	"path/filepath"
	"runtime"
)

// releases is where a release's archives are downloaded from. Tests point it elsewhere.
var releases = "https://github.com/agent-labs-dev/fastbrowse/releases/download"

const ownBinary = "set Options.BinaryPath or FASTBROWSE_BINARY to run a fastbrowse binary of your own"

// target is the release's name for this platform, as its archives and the npm packages are named.
func target() (string, error) {
	name := map[string]string{
		"darwin/arm64": "darwin-arm64", "darwin/amd64": "darwin-x64", "linux/arm64": "linux-arm64",
		"linux/amd64": "linux-x64", "windows/amd64": "win32-x64",
	}[runtime.GOOS+"/"+runtime.GOARCH]
	if name == "" {
		return "", fmt.Errorf("fastbrowse: there is no fastbrowse binary for %s/%s; %s", runtime.GOOS, runtime.GOARCH, ownBinary)
	}
	return name, nil
}

func executable() string {
	if runtime.GOOS == "windows" {
		return "fastbrowse.exe"
	}
	return "fastbrowse"
}

// resolveBinary is the executable to start: the caller's own path first, then the one the environment names, then
// the release binary, downloaded into the user cache directory the first time.
func resolveBinary(ctx context.Context, path string) (string, error) {
	if path != "" {
		return path, nil
	}
	if named := os.Getenv("FASTBROWSE_BINARY"); named != "" {
		return named, nil
	}
	cache, err := os.UserCacheDir()
	if err != nil {
		return "", fmt.Errorf("fastbrowse: no cache directory to keep the fastbrowse binary in (%w); %s", err, ownBinary)
	}
	return releaseBinary(ctx, filepath.Join(cache, "fastbrowse"))
}

// releaseBinary is the release binary under cache, downloaded and checked against the module's checksum when it is
// not there yet.
func releaseBinary(ctx context.Context, cache string) (string, error) {
	if releaseVersion == "" {
		return "", errors.New("fastbrowse: this copy of the module is not a release, so it pins no fastbrowse binary; " + ownBinary)
	}
	platform, err := target()
	if err != nil {
		return "", err
	}
	checksum := releaseChecksums[platform]
	if checksum == "" {
		return "", fmt.Errorf("fastbrowse: release %s has no binary for %s; %s", releaseVersion, platform, ownBinary)
	}
	directory := filepath.Join(cache, releaseVersion, platform)
	binary := filepath.Join(directory, executable())
	if _, err := os.Stat(binary); err == nil {
		return binary, nil
	}
	if err := os.MkdirAll(directory, 0o755); err != nil {
		return "", err
	}
	archive := fmt.Sprintf("%s/v%s/fastbrowse-rust-%s.tar.gz", releases, releaseVersion, platform)
	if err := download(ctx, archive, checksum, binary); err != nil {
		return "", fmt.Errorf("fastbrowse: could not get the fastbrowse binary from %s: %w", archive, err)
	}
	return binary, nil
}

// download fetches the archive at url, checks it against checksum, and writes the binary in it to binary. The binary
// is written beside its place and renamed into it, so a download that stops partway leaves nothing a later start
// would run.
func download(ctx context.Context, url, checksum, binary string) error {
	request, err := http.NewRequestWithContext(ctx, http.MethodGet, url, nil)
	if err != nil {
		return err
	}
	response, err := http.DefaultClient.Do(request)
	if err != nil {
		return err
	}
	defer response.Body.Close()
	if response.StatusCode != http.StatusOK {
		return fmt.Errorf("HTTP %s", response.Status)
	}
	archive, err := os.CreateTemp(filepath.Dir(binary), "download-*")
	if err != nil {
		return err
	}
	defer os.Remove(archive.Name())
	defer archive.Close()
	digest := sha256.New()
	if _, err := io.Copy(io.MultiWriter(archive, digest), response.Body); err != nil {
		return err
	}
	if got := hex.EncodeToString(digest.Sum(nil)); got != checksum {
		return fmt.Errorf("its SHA-256 is %s, and this module pins %s", got, checksum)
	}
	if _, err := archive.Seek(0, io.SeekStart); err != nil {
		return err
	}
	return extract(archive, binary)
}

// extract writes the archive's fastbrowse/<executable> to binary.
func extract(archive io.Reader, binary string) error {
	unzipped, err := gzip.NewReader(archive)
	if err != nil {
		return err
	}
	entries := tar.NewReader(unzipped)
	want := "fastbrowse/" + executable()
	for {
		header, err := entries.Next()
		if err == io.EOF {
			return fmt.Errorf("the archive holds no %s", want)
		}
		if err != nil {
			return err
		}
		if filepath.ToSlash(filepath.Clean(header.Name)) != want {
			continue
		}
		written, err := os.CreateTemp(filepath.Dir(binary), "binary-*")
		if err != nil {
			return err
		}
		defer os.Remove(written.Name())
		if _, err := io.Copy(written, entries); err != nil {
			written.Close()
			return err
		}
		if err := written.Close(); err != nil {
			return err
		}
		if err := os.Chmod(written.Name(), 0o755); err != nil {
			return err
		}
		if err := os.Rename(written.Name(), binary); err != nil {
			// Another start downloaded it first, and Windows will not rename over a file.
			if _, exists := os.Stat(binary); exists == nil {
				return nil
			}
			return err
		}
		return nil
	}
}
