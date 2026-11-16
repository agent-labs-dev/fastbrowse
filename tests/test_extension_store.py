import importlib.util
import io
import json
from pathlib import Path
from zipfile import ZipFile

import pytest

SPEC = importlib.util.spec_from_file_location(
    "extension_store", Path(__file__).parents[1] / "scripts/extension_store.py"
)
assert SPEC and SPEC.loader
store = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(store)


def revision(version="0.1.1", state="STAGED"):
    return {"state": state, "distributionChannels": [{"crxVersion": version}]}


def test_package_is_deterministic_and_preserves_identity(tmp_path):
    first, second = tmp_path / "one.zip", tmp_path / "two.zip"
    assert store.build(store.SOURCE, first) == store.build(store.SOURCE, second)
    assert first.read_bytes() == second.read_bytes()
    with ZipFile(first) as archive:
        manifest = json.loads(archive.read("manifest.json"))
        assert "key" not in manifest
        assert manifest["version"] == json.loads((store.SOURCE / "manifest.json").read_text())["version"]
        assert set(archive.namelist()) == {p.name for p in store.SOURCE.iterdir()}


def client(monkeypatch, responses):
    calls = []
    api = store.Store("publisher", "test-token")

    def request(action, payload=None, **kwargs):
        calls.append((action, payload, kwargs))
        return responses.pop(0)

    monkeypatch.setattr(api, "request", request)
    return api, calls


def status(**kwargs):
    return {"name": f"publishers/publisher/items/{store.ITEM}", "itemId": store.ITEM, **kwargs}


def test_submit_waits_for_upload_and_stages_review(tmp_path, monkeypatch):
    package = tmp_path / "package.zip"
    receipt = store.build(store.SOURCE, package)
    api, calls = client(
        monkeypatch,
        [
            status(),
            {"uploadState": "IN_PROGRESS"},
            status(lastAsyncUploadState="SUCCEEDED"),
            {"state": "PENDING_REVIEW"},
            status(submittedItemRevisionStatus=revision(receipt["version"], "PENDING_REVIEW")),
        ],
    )
    monkeypatch.setattr(store.time, "sleep", lambda _: None)
    assert api.submit(package, receipt["version"]) == {"state": "PENDING_REVIEW"}
    assert [c[0] for c in calls] == ["fetchStatus", "upload", "fetchStatus", "publish", "fetchStatus"]
    assert calls[1][2] == {"upload": True}
    assert json.loads(calls[-2][1]) == {"publishType": "STAGED_PUBLISH", "skipReview": False, "blockOnWarnings": True}


@pytest.mark.parametrize("state", ["FAILED", "NOT_FOUND", None])
def test_failed_upload_does_not_submit(tmp_path, monkeypatch, state):
    package = tmp_path / "package.zip"
    receipt = store.build(store.SOURCE, package)
    api, calls = client(monkeypatch, [status(), {"uploadState": state}])
    with pytest.raises(ValueError, match="upload did not succeed"):
        api.submit(package, receipt["version"])
    assert [c[0] for c in calls] == ["fetchStatus", "upload"]


@pytest.mark.parametrize("record", [revision(state="PENDING_REVIEW"), revision(version="0.1.0"), {}])
def test_publish_refuses_unapproved_or_different_versions(monkeypatch, record):
    api, calls = client(monkeypatch, [status(submittedItemRevisionStatus=record)])
    with pytest.raises(ValueError, match="not approved and staged"):
        api.publish("0.1.1")
    assert len(calls) == 1


def test_publish_only_releases_matching_staged_version(monkeypatch):
    published = revision(state="PUBLISHED")
    published["distributionChannels"][0]["deployPercentage"] = 100
    api, calls = client(
        monkeypatch,
        [
            status(submittedItemRevisionStatus=revision()),
            {"state": "PUBLISHED"},
            status(publishedItemRevisionStatus=published),
        ],
    )
    assert api.publish("0.1.1")["publishedItemRevisionStatus"] == published
    assert json.loads(calls[-2][1])["publishType"] == "DEFAULT_PUBLISH"


def test_existing_submission_is_not_overwritten(tmp_path, monkeypatch):
    package = tmp_path / "package.zip"
    receipt = store.build(store.SOURCE, package)
    api, calls = client(monkeypatch, [status(submittedItemRevisionStatus=revision())])
    with pytest.raises(ValueError, match="existing submission"):
        api.submit(package, receipt["version"])
    assert len(calls) == 1


def test_already_published_version_does_not_upload(tmp_path, monkeypatch):
    package = tmp_path / "package.zip"
    receipt = store.build(store.SOURCE, package)
    api, calls = client(monkeypatch, [status(publishedItemRevisionStatus=revision(receipt["version"], "PUBLISHED"))])
    with pytest.raises(ValueError, match="already published"):
        api.submit(package, receipt["version"])
    assert len(calls) == 1


@pytest.mark.parametrize(
    "version,percent,state", [("0.1.0", 100, "PUBLISHED"), ("0.1.1", 50, "PUBLISHED"), ("0.1.1", 100, "STAGED")]
)
def test_release_verification_refuses_incompatible_or_partial_rollout(monkeypatch, version, percent, state):
    record = revision(version, state)
    record["distributionChannels"][0]["deployPercentage"] = percent
    api, calls = client(monkeypatch, [status(publishedItemRevisionStatus=record)])
    with pytest.raises(ValueError, match="not published to every user"):
        api.verify("0.1.1")
    assert len(calls) == 1


def test_release_verification_accepts_matching_full_rollout(monkeypatch):
    record = revision(state="PUBLISHED")
    record["distributionChannels"][0]["deployPercentage"] = 100
    api, calls = client(monkeypatch, [status(publishedItemRevisionStatus=record)])
    assert api.verify("0.1.1")["publishedItemRevisionStatus"] == record
    assert len(calls) == 1


def test_transport_uses_upload_endpoint_and_bearer_header(monkeypatch):
    requests = []

    class Opener:
        def open(self, request, timeout):
            requests.append(request)
            assert timeout == 60
            return io.BytesIO(b'{"uploadState":"SUCCEEDED"}')

    monkeypatch.setattr(store.urllib.request, "build_opener", lambda *_: Opener())
    api = store.Store("publisher", "private-test-token")
    assert api.request("upload", b"zip-bytes", upload=True) == {"uploadState": "SUCCEEDED"}
    request = requests[0]
    assert (
        request.full_url
        == f"https://chromewebstore.googleapis.com/upload/v2/publishers/publisher/items/{store.ITEM}:upload"
    )
    assert request.method == "POST" and request.data == b"zip-bytes"
    assert request.get_header("Authorization") == "Bearer private-test-token"


def test_http_failure_does_not_echo_provider_body_or_token(monkeypatch):
    class Opener:
        def open(self, request, timeout):
            raise store.urllib.error.HTTPError(request.full_url, 403, "private-test-token", {}, None)

    monkeypatch.setattr(store.urllib.request, "build_opener", lambda *_: Opener())
    with pytest.raises(ValueError, match="HTTP 403") as error:
        store.Store("publisher", "private-test-token").status()
    assert "private-test-token" not in str(error.value)


def test_redirect_cannot_forward_the_token():
    with pytest.raises(ValueError, match="redirects are refused"):
        store.NoRedirect().redirect_request(None, None, 302, "", {}, "https://example.com/")


def test_unexpected_publisher_path_is_refused():
    with pytest.raises(ValueError, match="publisher ID"):
        store.Store("publisher/another", "token")


@pytest.mark.parametrize("response", [{}, {"state": "REJECTED"}, {"state": "PUBLISHED"}])
def test_submit_requires_a_staged_review_response(tmp_path, monkeypatch, response):
    package = tmp_path / "package.zip"
    receipt = store.build(store.SOURCE, package)
    api, _ = client(monkeypatch, [status(), {"uploadState": "SUCCEEDED"}, response])
    with pytest.raises(ValueError, match="did not accept a staged review"):
        api.submit(package, receipt["version"])


def test_submit_detects_a_different_revision_after_upload(tmp_path, monkeypatch):
    package = tmp_path / "package.zip"
    receipt = store.build(store.SOURCE, package)
    api, _ = client(
        monkeypatch,
        [
            status(),
            {"uploadState": "SUCCEEDED"},
            {"state": "PENDING_REVIEW"},
            status(submittedItemRevisionStatus=revision("0.1.0")),
        ],
    )
    with pytest.raises(ValueError, match="submission version differs"):
        api.submit(package, receipt["version"])


def test_async_upload_timeout_never_submits(tmp_path, monkeypatch):
    package = tmp_path / "package.zip"
    receipt = store.build(store.SOURCE, package)
    api, calls = client(monkeypatch, [status(), {"uploadState": "IN_PROGRESS"}])
    clock = iter([0, 301])
    monkeypatch.setattr(store.time, "monotonic", lambda: next(clock))
    with pytest.raises(ValueError, match="upload did not succeed"):
        api.submit(package, receipt["version"])
    assert [call[0] for call in calls] == ["fetchStatus", "upload"]
