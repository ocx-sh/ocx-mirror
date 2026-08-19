"""Acceptance coverage for `ocx-mirror dist sync`.

`dist sync` mirrors the *bootstrap* layer — ocx's own release archives and the
`dist.json` manifest — into a generic store, and touches no OCI registry at
all. Every test here therefore runs against a plain HTTP server and needs no
Docker registry, which is why the runner is built locally instead of through
the registry-bound `mirror` fixture.

The load-bearing test is
`the_emitted_manifest_is_readable_by_the_posix_installer_parse`: it runs the
exact `grep`/`sed` pipeline `www-setup/src/install.sh` uses, so a rendering
change that breaks every jq-free installer fails here rather than in the field.
"""
from __future__ import annotations

import base64
import hashlib
import http.server
import json
import subprocess
import threading
from pathlib import Path

import pytest

from src.mirror_runner import MirrorRunner

# ---------------------------------------------------------------------------
# Fixtures
# ---------------------------------------------------------------------------


@pytest.fixture()
def dist_mirror(mirror_binary: Path, tmp_path: Path) -> MirrorRunner:
    """A runner with no registry dependency.

    `dist sync` never dials a registry, so binding these tests to the session
    registry fixture would make them need Docker for no reason — and would
    exclude them from an `OCX_TESTS_NO_REGISTRY=1` selection.
    """
    work = tmp_path / "dist-work"
    work.mkdir()
    return MirrorRunner(mirror_binary, "localhost:5001", work)


class UploadCapture:
    """A store that records every request and answers 404 until written."""

    def __init__(self, port: int, stored: dict[str, bytes], requests: list[tuple[str, str]]):
        self.port = port
        self.base_url = f"http://127.0.0.1:{port}"
        self.stored = stored
        self.requests = requests

    def paths(self, method: str) -> list[str]:
        return [path for verb, path in self.requests if verb == method]

    def headers_seen(self) -> list[dict[str, str]]:
        return self._headers

    def requests_with_headers(self, method: str) -> list[tuple[str, dict[str, str]]]:
        """Every `(path, headers)` for one verb, in wire order.

        The two recording lists append together on every request, so they zip
        positionally — which is what lets a test assert a header *against the
        body that carried it* rather than merely that some request had one.
        """
        return [
            (path, headers)
            for (verb, path), headers in zip(self.requests, self._headers, strict=True)
            if verb == method
        ]

    def url(self, suffix: str = "") -> str:
        return f"{self.base_url}{suffix}"


@pytest.fixture()
def upload_store(request: pytest.FixtureRequest):
    """A minimal PUT/HEAD store, standing in for an Artifactory generic repo.

    Parametrise with `@pytest.mark.upload_store(fail_puts=N, status=S)` to make
    the first N `PUT`s answer `S` before the store starts accepting — the only
    way to exercise `with_retries` end to end through the real HTTP client.
    """
    marker = request.node.get_closest_marker("upload_store")
    options = marker.kwargs if marker else {}
    remaining_failures = {"count": options.get("fail_puts", 0)}
    failure_status = options.get("status", 503)
    # `omit_checksum=True` stands in for a plain WebDAV or S3-alike that answers
    # a HEAD with no digest at all — the store the probe must degrade against
    # rather than break on.
    omit_checksum = options.get("omit_checksum", False)

    stored: dict[str, bytes] = {}
    requests: list[tuple[str, str]] = []
    headers: list[dict[str, str]] = []

    class Handler(http.server.BaseHTTPRequestHandler):
        def do_HEAD(self):  # noqa: N802 — BaseHTTPRequestHandler's spelling
            requests.append(("HEAD", self.path))
            # Lower-cased: hyper writes header names in lower case on the
            # wire, and a plain dict() of the parsed message loses the
            # case-insensitive lookup `email.message.Message` provides.
            headers.append({name.lower(): value for name, value in self.headers.items()})
            body = stored.get(self.path)
            self.send_response(404 if body is None else 200)
            # Artifactory, Nexus and GitLab all answer a HEAD with the digest
            # they hold. That is what lets the run skip an archive without
            # downloading it, and skip it *verified* rather than on the mere
            # existence of a path — so the harness has to answer like a real
            # store or the probe path is never exercised.
            if body is not None and not omit_checksum:
                self.send_header("X-Checksum-Sha256", hashlib.sha256(body).hexdigest())
            self.end_headers()

        def do_PUT(self):  # noqa: N802
            requests.append(("PUT", self.path))
            # Lower-cased: hyper writes header names in lower case on the
            # wire, and a plain dict() of the parsed message loses the
            # case-insensitive lookup `email.message.Message` provides.
            headers.append({name.lower(): value for name, value in self.headers.items()})
            length = int(self.headers.get("Content-Length", "0"))
            body = self.rfile.read(length)

            if remaining_failures["count"] > 0:
                remaining_failures["count"] -= 1
                self.send_response(failure_status)
                self.end_headers()
                return

            stored[self.path] = body
            self.send_response(201)
            self.end_headers()

        def log_message(self, *args):
            return

    httpd = http.server.HTTPServer(("127.0.0.1", 0), Handler)
    thread = threading.Thread(target=httpd.serve_forever, daemon=True)
    thread.start()

    capture = UploadCapture(httpd.server_address[1], stored, requests)
    capture._headers = headers
    yield capture
    httpd.shutdown()


# ---------------------------------------------------------------------------
# Upstream fixture construction
# ---------------------------------------------------------------------------

TARGETS = ("x86_64-unknown-linux-gnu", "aarch64-apple-darwin")


def _archive_bytes(version: str, target: str) -> bytes:
    return f"ocx {version} for {target}\n".encode()


def publish_upstream(asset_server, versions: list[tuple[str, str]]) -> str:
    """Write archives plus a `dist.json` naming them, and return its URL.

    `versions` is `[(version, channel), ...]`, newest-first, matching the
    ordering `gen-dist.sh` emits.
    """
    releases = []
    for version, channel in versions:
        tag = f"v{version}"
        for target in TARGETS:
            filename = f"ocx-{target}.tar.gz"
            payload = _archive_bytes(version, target)
            directory = asset_server.dir / tag
            directory.mkdir(parents=True, exist_ok=True)
            (directory / filename).write_bytes(payload)
            releases.append(
                {
                    "version": version,
                    "channel": channel,
                    "tag": tag,
                    "target": target,
                    "filename": filename,
                    "sha256": hashlib.sha256(payload).hexdigest(),
                    "url": asset_server.url(f"{tag}/{filename}"),
                }
            )

    def pointer(channel: str):
        for version, row_channel in versions:
            if row_channel == channel:
                return {"version": version, "channel": channel}
        return None

    # One leaf object per line, exactly as `gen-dist.sh` emits it.
    lines = [
        "{",
        '  "schema": 1,',
        f'  "latest": {json.dumps(pointer("stable"), separators=(",", ":"))},',
        f'  "latest_next": {json.dumps(pointer("next"), separators=(",", ":"))},',
        '  "releases": [',
    ]
    for index, row in enumerate(releases):
        separator = "" if index + 1 == len(releases) else ","
        lines.append(f"    {json.dumps(row, separators=(',', ':'))}{separator}")
    lines += ["  ]", "}"]
    (asset_server.dir / "dist.json").write_text("\n".join(lines) + "\n")
    return asset_server.url("dist.json")


def write_spec(
    path: Path,
    source: str,
    output: Path,
    *,
    base_url: str = "https://art.corp.test/ocx-dist",
    layout: str | None = None,
    min_version: str | None = None,
    upload: bool = False,
    retry_delays: list[int] | None = None,
    max_downloads: int | None = None,
    max_uploads: int | None = None,
) -> Path:
    body = [
        "kind: dist",
        f"source: {source}",
        f"output: {output}",
        "trusted_hosts:",
        "  - 127.0.0.1",
        "publish:",
        f"  base_url: {base_url}",
    ]
    if layout:
        body.append(f"  layout: '{layout}'")
    if min_version:
        body += ["select:", f"  min_version: '{min_version}'"]
    if upload:
        body += [
            "upload:",
            "  identity:",
            "    type: basic",
            "    username_env: DIST_USER",
            "    password_env: DIST_PASSWORD",
            f"  retry_delays: {retry_delays if retry_delays is not None else []}",
        ]
    if max_downloads is not None or max_uploads is not None:
        body.append("concurrency:")
        if max_downloads is not None:
            body.append(f"  max_downloads: {max_downloads}")
        if max_uploads is not None:
            body.append(f"  max_uploads: {max_uploads}")
    path.write_text("\n".join(body) + "\n")
    return path


# ---------------------------------------------------------------------------
# Tests
# ---------------------------------------------------------------------------


def test_the_mirror_tree_carries_every_archive_and_the_manifest(dist_mirror, asset_server, tmp_path):
    source = publish_upstream(asset_server, [("0.5.8", "stable")])
    output = tmp_path / "public"
    spec = write_spec(tmp_path / "dist.yml", source, output)

    dist_mirror.run("dist", "sync", str(spec))

    for target in TARGETS:
        archive = output / "v0.5.8" / f"ocx-{target}.tar.gz"
        assert archive.read_bytes() == _archive_bytes("0.5.8", target)

    manifest = json.loads((output / "dist.json").read_text())
    assert manifest["schema"] == 1
    assert manifest["latest"] == {"version": "0.5.8", "channel": "stable"}
    assert len(manifest["releases"]) == len(TARGETS)


def test_every_manifest_url_points_at_the_mirror(dist_mirror, asset_server, tmp_path):
    source = publish_upstream(asset_server, [("0.5.8", "stable")])
    output = tmp_path / "public"
    spec = write_spec(tmp_path / "dist.yml", source, output)

    dist_mirror.run("dist", "sync", str(spec))

    manifest = json.loads((output / "dist.json").read_text())
    for row in manifest["releases"]:
        assert row["url"] == f"https://art.corp.test/ocx-dist/v0.5.8/{row['filename']}"


def test_the_upstream_digest_is_preserved_rather_than_recomputed(dist_mirror, asset_server, tmp_path):
    source = publish_upstream(asset_server, [("0.5.8", "stable")])
    output = tmp_path / "public"
    spec = write_spec(tmp_path / "dist.yml", source, output)
    upstream = json.loads((asset_server.dir / "dist.json").read_text())

    dist_mirror.run("dist", "sync", str(spec))

    mirrored = json.loads((output / "dist.json").read_text())
    assert [row["sha256"] for row in mirrored["releases"]] == [row["sha256"] for row in upstream["releases"]]


def test_the_emitted_manifest_is_readable_by_the_posix_installer_parse(dist_mirror, asset_server, tmp_path):
    """The exact pipeline `www-setup/src/install.sh` runs.

    `get_latest_version` extracts leaf objects with `grep -o '{[^{}]*}'`, keeps
    the first whose channel is stable, and seds the version out of it. `grep`
    is line-based, so a manifest rendered with a pretty-printer resolves
    nothing — silently, on every shell.
    """
    source = publish_upstream(asset_server, [("0.6.0-rc.1", "next"), ("0.5.8", "stable")])
    output = tmp_path / "public"
    spec = write_spec(tmp_path / "dist.yml", source, output)

    dist_mirror.run("dist", "sync", str(spec))

    resolved = subprocess.run(
        "grep -o '{[^{}]*}' "
        "| grep '\"channel\"[[:space:]]*:[[:space:]]*\"stable\"' "
        "| head -1 "
        "| sed -n 's/.*\"version\"[[:space:]]*:[[:space:]]*\"\\([^\"]*\\)\".*/\\1/p' "
        "| head -1",
        shell=True,
        input=(output / "dist.json").read_text(),
        capture_output=True,
        text=True,
        check=True,
    ).stdout.strip()

    assert resolved == "0.5.8", "the installer's own parse must resolve the latest stable version"


def test_the_installer_parse_finds_the_row_for_a_version_and_target(dist_mirror, asset_server, tmp_path):
    """`dist_row` greps a single leaf object by version *and* target."""
    source = publish_upstream(asset_server, [("0.5.8", "stable")])
    output = tmp_path / "public"
    spec = write_spec(tmp_path / "dist.yml", source, output)

    dist_mirror.run("dist", "sync", str(spec))

    row = subprocess.run(
        "grep -o '{[^{}]*}' "
        '| grep \'"version"[[:space:]]*:[[:space:]]*"0.5.8"\' '
        '| grep \'"target"[[:space:]]*:[[:space:]]*"x86_64-unknown-linux-gnu"\' '
        "| head -1",
        shell=True,
        input=(output / "dist.json").read_text(),
        capture_output=True,
        text=True,
        check=True,
    ).stdout.strip()

    assert row.startswith("{") and row.endswith("}")
    assert "x86_64-unknown-linux-gnu" in row
    assert "art.corp.test" in row, "the row the installer downloads from must name the mirror"


def test_a_min_version_bound_drops_older_releases_and_repoints_latest(dist_mirror, asset_server, tmp_path):
    source = publish_upstream(asset_server, [("0.5.8", "stable"), ("0.4.0", "stable")])
    output = tmp_path / "public"
    spec = write_spec(tmp_path / "dist.yml", source, output, min_version="0.5.0")

    dist_mirror.run("dist", "sync", str(spec))

    manifest = json.loads((output / "dist.json").read_text())
    assert {row["version"] for row in manifest["releases"]} == {"0.5.8"}
    assert manifest["latest"]["version"] == "0.5.8"
    assert not (output / "v0.4.0").exists(), "a filtered release must not be downloaded"


def test_a_bound_that_empties_a_channel_leaves_an_explicit_null(dist_mirror, asset_server, tmp_path):
    # `0.6.0` excludes its own release candidate (a prerelease sorts below the
    # release), so the `next` channel empties while `stable` survives on 0.7.0.
    source = publish_upstream(
        asset_server, [("0.7.0", "stable"), ("0.6.0-rc.1", "next"), ("0.5.8", "stable")]
    )
    output = tmp_path / "public"
    spec = write_spec(tmp_path / "dist.yml", source, output, min_version="0.6.0")

    dist_mirror.run("dist", "sync", str(spec))

    manifest = json.loads((output / "dist.json").read_text())
    assert manifest["latest"]["version"] == "0.7.0"
    assert manifest["latest_next"] is None, "an emptied channel is an explicit null, never an omitted key"
    assert "latest_next" in manifest


def test_a_bound_that_empties_every_channel_publishes_nothing(dist_mirror, asset_server, tmp_path):
    """The other half of clobber-safety.

    A run where every release was filtered out has trivially "placed every
    selected archive", so the partial-run guard reads it as a success. Left
    alone it would overwrite a working `dist.json` with one naming nothing,
    and every consumer resolving `latest` would get `null`. The reachable
    cause is a `min_version` typo, which nothing else in the run would flag.
    """
    source = publish_upstream(asset_server, [("0.5.8", "stable")])
    output = tmp_path / "public"
    spec = write_spec(tmp_path / "dist.yml", source, output, min_version="9.0.0")

    result = dist_mirror.run("dist", "sync", str(spec), check=False)

    assert result.returncode != 0
    assert not (output / "dist.json").exists(), "an empty selection must publish no manifest"
    assert "no release survived" in (result.stderr + result.stdout)


def test_the_content_addressed_snapshot_matches_the_rolling_manifest(dist_mirror, asset_server, tmp_path):
    source = publish_upstream(asset_server, [("0.5.8", "stable")])
    output = tmp_path / "public"
    spec = write_spec(tmp_path / "dist.yml", source, output)

    dist_mirror.run("dist", "sync", str(spec))

    rolling = (output / "dist.json").read_bytes()
    digest = hashlib.sha256(rolling).hexdigest()
    assert (output / "dist" / f"{digest}.json").read_bytes() == rolling
    # No `dist.json.sha256`. Nothing ever read one — install.sh verifies each
    # archive against the manifest's own inline sha256 — and a `*.sha256` path
    # is not a file Artifactory will store, so it could not be served either.
    assert not (output / "dist.json.sha256").exists()


def test_an_unchanged_upstream_produces_the_same_snapshot_name(dist_mirror, asset_server, tmp_path):
    """Content addressing is what makes a re-run cheap: no new file appears."""
    source = publish_upstream(asset_server, [("0.5.8", "stable")])
    output = tmp_path / "public"
    spec = write_spec(tmp_path / "dist.yml", source, output)

    dist_mirror.run("dist", "sync", str(spec))
    first = sorted(p.name for p in (output / "dist").iterdir())
    dist_mirror.run("dist", "sync", str(spec))
    second = sorted(p.name for p in (output / "dist").iterdir())

    assert first == second == [n for n in first]
    assert len(second) == 1, "an unchanged manifest must not accumulate snapshots"


def test_a_second_run_skips_archives_it_already_holds(dist_mirror, asset_server, tmp_path):
    source = publish_upstream(asset_server, [("0.5.8", "stable")])
    output = tmp_path / "public"
    spec = write_spec(tmp_path / "dist.yml", source, output)

    dist_mirror.run("dist", "sync", str(spec))
    result = dist_mirror.run("dist", "sync", str(spec), "--format", "json")

    report = json.loads(result.stdout)
    assert report["counters"]["skipped"] == len(TARGETS)
    assert report["counters"]["copied"] == 0


def test_a_digest_mismatch_fails_the_run_and_publishes_no_manifest(dist_mirror, asset_server, tmp_path):
    """Clobber-safety: the store keeps its previous, consistent manifest."""
    source = publish_upstream(asset_server, [("0.5.8", "stable")])
    manifest = json.loads((asset_server.dir / "dist.json").read_text())
    manifest["releases"][0]["sha256"] = "0" * 64
    (asset_server.dir / "dist.json").write_text(json.dumps(manifest))
    output = tmp_path / "public"
    spec = write_spec(tmp_path / "dist.yml", source, output)

    result = dist_mirror.run("dist", "sync", str(spec), check=False)

    assert result.returncode != 0
    assert not (output / "dist.json").exists(), "a partial run must publish no manifest"
    assert not (output / "dist").exists(), "a partial run must publish no snapshot either"


def test_a_credential_gated_source_is_read_with_host_keyed_credentials(dist_mirror, asset_server, tmp_path):
    """`upload.identity` covers writes; a store that also gates *reads* is served here.

    The credential is host-keyed (`OCX_AUTH_<slug>_*`, the slug being the host
    with every non-alphanumeric byte replaced) and appears nowhere in the spec —
    the same resolver the archive downloads use, so one variable covers the
    manifest fetch and every byte it names.
    """
    publish_upstream(asset_server, [("0.5.8", "stable")])

    class GatedHandler(http.server.SimpleHTTPRequestHandler):
        def send_head(self):
            expected = "Basic " + base64.b64encode(b"ci-mirror:s3cr3t").decode()
            if self.headers.get("Authorization") != expected:
                self.send_response(401)
                self.send_header("WWW-Authenticate", 'Basic realm="dist"')
                self.send_header("Content-Length", "0")
                self.end_headers()
                return None
            return super().send_head()

        def log_message(self, fmt, *args):
            pass

    gated = http.server.HTTPServer(
        ("127.0.0.1", 0),
        lambda *args: GatedHandler(*args, directory=str(asset_server.dir)),
    )
    threading.Thread(target=gated.serve_forever, daemon=True).start()
    try:
        source = f"http://127.0.0.1:{gated.server_address[1]}/dist.json"
        output = tmp_path / "public"
        spec = write_spec(tmp_path / "dist.yml", source, output)

        # A developer's own ~/.netrc must not stand in for the credential.
        dist_mirror.env["NETRC"] = str(tmp_path / "no-such-netrc")
        anonymous = dist_mirror.run("dist", "sync", str(spec), check=False)
        published_anonymously = (output / "dist.json").exists()

        dist_mirror.env["OCX_AUTH_127_0_0_1_USER"] = "ci-mirror"
        dist_mirror.env["OCX_AUTH_127_0_0_1_TOKEN"] = "s3cr3t"
        authenticated = dist_mirror.run("dist", "sync", str(spec), check=False)
    finally:
        gated.shutdown()

    assert anonymous.returncode != 0, "an unauthenticated read must fail the run"
    assert not published_anonymously, "a refused source must publish nothing"
    assert authenticated.returncode == 0, (
        f"the credentialed read must succeed (rc={authenticated.returncode})\nstderr: {authenticated.stderr}"
    )
    assert (output / "dist.json").exists(), "the credentialed run publishes the manifest"
    assert "s3cr3t" not in authenticated.stderr, "no credential may reach stderr"


def test_a_manifest_row_that_would_escape_the_output_directory_is_refused(dist_mirror, asset_server, tmp_path):
    source = publish_upstream(asset_server, [("0.5.8", "stable")])
    manifest = json.loads((asset_server.dir / "dist.json").read_text())
    manifest["releases"][0]["filename"] = "../../escaped.tar.gz"
    (asset_server.dir / "dist.json").write_text(json.dumps(manifest))
    output = tmp_path / "public"
    spec = write_spec(tmp_path / "dist.yml", source, output)

    result = dist_mirror.run("dist", "sync", str(spec), check=False)

    assert result.returncode != 0
    assert not (tmp_path / "escaped.tar.gz").exists()
    assert not (tmp_path.parent / "escaped.tar.gz").exists()


def test_a_dry_run_writes_nothing(dist_mirror, asset_server, tmp_path):
    source = publish_upstream(asset_server, [("0.5.8", "stable")])
    output = tmp_path / "public"
    spec = write_spec(tmp_path / "dist.yml", source, output)

    result = dist_mirror.run("dist", "sync", str(spec), "--dry-run", "--format", "json")

    report = json.loads(result.stdout)
    assert report["dry_run"] is True
    assert report["counters"]["total"] == len(TARGETS)
    assert not output.exists() or not any(output.iterdir())


def test_a_spec_of_the_wrong_kind_is_refused_with_a_usage_error(dist_mirror, tmp_path):
    spec = tmp_path / "dist.yml"
    spec.write_text("kind: registry\noutput: ./public\n")

    result = dist_mirror.run("dist", "sync", str(spec), check=False)

    assert result.returncode == 64, f"a wrong-kind document is a usage error: {result.stderr}"
    assert "dist" in result.stderr


def test_an_unsupported_schema_is_refused_without_writing(dist_mirror, asset_server, tmp_path):
    source = publish_upstream(asset_server, [("0.5.8", "stable")])
    manifest = json.loads((asset_server.dir / "dist.json").read_text())
    manifest["schema"] = 2
    (asset_server.dir / "dist.json").write_text(json.dumps(manifest))
    output = tmp_path / "public"
    spec = write_spec(tmp_path / "dist.yml", source, output)

    result = dist_mirror.run("dist", "sync", str(spec), check=False)

    assert result.returncode == 65, f"a newer schema is a data error, not a transient one: {result.stderr}"
    assert not (output / "dist.json").exists()


def test_an_oversize_upstream_manifest_is_refused_before_it_is_parsed(dist_mirror, asset_server, tmp_path):
    """CWE-400: the manifest is read before anything about it has been checked.

    The padding rides in an unknown top-level key, so the document stays valid
    `schema: 1` — a refusal here is the size cap, not a parse failure.
    """
    source = publish_upstream(asset_server, [("0.5.8", "stable")])
    manifest = json.loads((asset_server.dir / "dist.json").read_text())
    manifest["padding"] = "A" * (9 * 1024 * 1024)
    (asset_server.dir / "dist.json").write_text(json.dumps(manifest))
    output = tmp_path / "public"
    spec = write_spec(tmp_path / "dist.yml", source, output)

    result = dist_mirror.run("dist", "sync", str(spec), check=False)

    assert result.returncode == 69, f"an oversize body is an unusable source: {result.stderr}"
    assert "cap" in result.stderr
    assert not (output / "dist.json").exists()


def test_the_gitlab_layout_changes_both_the_tree_and_the_manifest_urls(dist_mirror, asset_server, tmp_path):
    """A GitLab generic package registry addresses files as `<version>/<file>`."""
    source = publish_upstream(asset_server, [("0.5.8", "stable")])
    output = tmp_path / "public"
    spec = write_spec(
        tmp_path / "dist.yml",
        source,
        output,
        base_url="https://gitlab.corp.test/api/v4/projects/42/packages/generic/ocx",
        layout="{version}/{filename}",
    )

    dist_mirror.run("dist", "sync", str(spec))

    assert (output / "0.5.8" / "ocx-x86_64-unknown-linux-gnu.tar.gz").is_file()
    manifest = json.loads((output / "dist.json").read_text())
    for row in manifest["releases"]:
        assert row["url"] == (
            f"https://gitlab.corp.test/api/v4/projects/42/packages/generic/ocx/0.5.8/{row['filename']}"
        )


def test_upload_puts_the_rolling_manifest_after_every_archive_it_names(
    dist_mirror, asset_server, upload_store, tmp_path
):
    """The mid-run guarantee: a consumer resolves the old manifest or the new one.

    Also pins that no `*.sha256` path is uploaded at all — Artifactory does not
    store one, and nothing reads one.
    """
    source = publish_upstream(asset_server, [("0.5.8", "stable")])
    output = tmp_path / "public"
    spec = write_spec(tmp_path / "dist.yml", source, output, base_url=upload_store.url(), upload=True)
    dist_mirror.env["DIST_USER"] = "ci"
    dist_mirror.env["DIST_PASSWORD"] = "hunter2"

    dist_mirror.run("dist", "sync", str(spec))

    puts = upload_store.paths("PUT")
    assert puts[-1] == "/dist.json", f"the rolling manifest must be published last: {puts}"
    assert not any(path.endswith(".sha256") for path in puts), f"no checksum sidecar is published: {puts}"
    assert any(path.startswith("/dist/") for path in puts), "the snapshot must be published"
    manifest_index = puts.index("/dist.json")
    archives = [path for path in puts if path.startswith("/v0.5.8/")]
    assert archives, "archives must be uploaded"
    for archive in archives:
        assert puts.index(archive) < manifest_index, "every archive precedes the manifest naming it"


def test_every_upload_announces_all_four_checksums_of_its_own_body(
    dist_mirror, asset_server, upload_store, tmp_path
):
    """Artifactory records a client checksum per algorithm.

    A header that is merely *present on some request* is not enough: the panel
    shows "Client did not publish a checksum value" for every algorithm whose
    header was absent, so each PUT must carry all four — and each value must
    hash the body that request actually sent, not a neighbouring row's.
    """
    source = publish_upstream(asset_server, [("0.5.8", "stable")])
    output = tmp_path / "public"
    spec = write_spec(tmp_path / "dist.yml", source, output, base_url=upload_store.url(), upload=True)
    dist_mirror.env["DIST_USER"] = "ci"
    dist_mirror.env["DIST_PASSWORD"] = "hunter2"

    dist_mirror.run("dist", "sync", str(spec))

    puts = upload_store.requests_with_headers("PUT")
    assert puts, "the run must have uploaded something"
    assert all(headers.get("authorization", "").startswith("Basic ") for _, headers in puts)

    algorithms = {
        "x-checksum-md5": hashlib.md5,
        "x-checksum-sha1": hashlib.sha1,
        "x-checksum-sha256": hashlib.sha256,
        "x-checksum-sha512": hashlib.sha512,
    }
    for path, headers in puts:
        body = upload_store.stored[path]
        for header, algorithm in algorithms.items():
            assert header in headers, f"{path} was uploaded without {header}"
            assert headers[header] == algorithm(body).hexdigest(), (
                f"{path}'s {header} does not hash the body it was sent with"
            )


def test_a_second_upload_skips_the_archives_but_republishes_the_rolling_manifest(
    dist_mirror, asset_server, upload_store, tmp_path
):
    """HEAD-then-skip is right for content-addressed paths and wrong for rolling ones.

    An archive and the `dist/<sha256>.json` snapshot are pinned by their names,
    so "already there" means "already correct". `dist.json` is not: the path
    outlives its contents, and skipping it freezes the store's manifest at
    whatever the first run wrote while fresh archives keep landing beside it.
    """
    source = publish_upstream(asset_server, [("0.5.8", "stable")])
    output = tmp_path / "public"
    spec = write_spec(tmp_path / "dist.yml", source, output, base_url=upload_store.url(), upload=True)
    dist_mirror.env["DIST_USER"] = "ci"
    dist_mirror.env["DIST_PASSWORD"] = "hunter2"

    dist_mirror.run("dist", "sync", str(spec))
    first = len(upload_store.paths("PUT"))
    result = dist_mirror.run("dist", "sync", str(spec), "--format", "json")

    report = json.loads(result.stdout)
    second = upload_store.paths("PUT")[first:]
    assert second == ["/dist.json"], f"only the rolling manifest may be re-uploaded: {second}"
    assert report["counters"]["uploaded"] == 1
    assert report["counters"]["already_present"] == first - 1, "every content-addressed path is skipped"


def test_a_new_upstream_release_republishes_the_rolling_manifest(
    dist_mirror, asset_server, upload_store, tmp_path
):
    """The regression the rolling/immutable split exists to prevent.

    With `dist.json` skipped, the store keeps serving the first run's manifest
    while the second run's archives land beside it — a manifest that names
    fewer releases than the store actually holds, and nothing in the report
    looks wrong.
    """
    publish_upstream(asset_server, [("0.5.8", "stable")])
    output = tmp_path / "public"
    spec = write_spec(
        tmp_path / "dist.yml", asset_server.url("dist.json"), output, base_url=upload_store.url(), upload=True
    )
    dist_mirror.env["DIST_USER"] = "ci"
    dist_mirror.env["DIST_PASSWORD"] = "hunter2"
    dist_mirror.run("dist", "sync", str(spec))

    # Upstream gains a release; the rolling manifest must follow it.
    publish_upstream(asset_server, [("0.5.9", "stable"), ("0.5.8", "stable")])
    dist_mirror.run("dist", "sync", str(spec))

    served = json.loads(upload_store.stored["/dist.json"])
    assert {row["version"] for row in served["releases"]} == {"0.5.8", "0.5.9"}, (
        "the store must serve the manifest naming every archive it now holds"
    )

    digest = hashlib.sha256(upload_store.stored["/dist.json"]).hexdigest()
    assert f"/dist/{digest}.json" in upload_store.paths("PUT"), "the new snapshot must be published too"
    assert upload_store.stored[f"/dist/{digest}.json"] == upload_store.stored["/dist.json"], (
        "the snapshot an operator pins must be the manifest the store serves"
    )


def test_the_download_width_changes_nothing_about_the_result(dist_mirror, asset_server, tmp_path):
    """`concurrency:` is a throughput knob, never a correctness one.

    `buffered` yields in input order precisely so the report does not depend on
    which archive finishes first; a serial run and a wide one must be
    byte-identical in both the tree and the report.
    """
    source = publish_upstream(asset_server, [("0.5.9", "stable"), ("0.5.8", "stable")])

    def run(name: str, **concurrency) -> tuple[dict, dict[str, bytes]]:
        output = tmp_path / name
        spec = write_spec(tmp_path / f"{name}.yml", source, output, **concurrency)
        result = dist_mirror.run("dist", "sync", str(spec), "--format", "json")
        tree = {
            str(path.relative_to(output)): path.read_bytes() for path in sorted(output.rglob("*")) if path.is_file()
        }
        return json.loads(result.stdout), tree

    serial_report, serial_tree = run("serial", max_downloads=1)
    wide_report, wide_tree = run("wide", max_downloads=8)

    assert serial_tree == wide_tree, "the emitted tree must not depend on download width"
    assert [entry["name"] for entry in serial_report["archives"]] == [
        entry["name"] for entry in wide_report["archives"]
    ], "the report must stay in manifest order whatever order the fetches finish in"
    assert serial_report["counters"] == wide_report["counters"]


def test_a_missing_credential_variable_fails_before_the_first_byte_moves(
    dist_mirror, asset_server, upload_store, tmp_path
):
    """Credentials resolve before the download pass, not at the upload pass.

    Resolving them late would spend a multi-gigabyte download before reporting
    a typo in an environment variable name, and the operator would have to
    repeat it.
    """
    source = publish_upstream(asset_server, [("0.5.8", "stable")])
    output = tmp_path / "public"
    spec = write_spec(tmp_path / "dist.yml", source, output, base_url=upload_store.url(), upload=True)

    result = dist_mirror.run("dist", "sync", str(spec), check=False)

    assert result.returncode != 0
    assert "DIST_USER" in result.stderr
    assert not upload_store.paths("PUT"), "nothing may be uploaded once credentials are known to be missing"
    assert not output.exists() or not any(output.iterdir()), "no archive may be downloaded either"
    assert not asset_server.requests, "the upstream manifest must not even be fetched"


@pytest.mark.upload_store(fail_puts=2, status=503)
def test_a_transient_5xx_is_retried_until_the_store_accepts(dist_mirror, asset_server, upload_store, tmp_path):
    """End-to-end cover for the retry loop, which no unit test reaches.

    The store rejects the first two `PUT`s with 503, then accepts. A run that
    stopped retrying — or miscounted its attempts — fails here.
    """
    source = publish_upstream(asset_server, [("0.5.8", "stable")])
    output = tmp_path / "public"
    spec = write_spec(
        tmp_path / "dist.yml",
        source,
        output,
        base_url=upload_store.url(),
        upload=True,
        retry_delays=[1, 1, 1],
    )
    dist_mirror.env["DIST_USER"] = "ci"
    dist_mirror.env["DIST_PASSWORD"] = "hunter2"

    result = dist_mirror.run("dist", "sync", str(spec), "--format", "json")

    report = json.loads(result.stdout)
    puts = upload_store.paths("PUT")
    assert len(puts) == report["counters"]["uploaded"] + 2, (
        f"two rejected attempts must be retried, not counted as uploads: {puts}"
    )
    assert "/dist.json" in puts, "the run must still complete through the manifest"


@pytest.mark.upload_store(fail_puts=99, status=401)
def test_an_unauthorized_upload_is_never_retried(dist_mirror, asset_server, upload_store, tmp_path):
    """A 401 is a credential problem a retry cannot solve.

    Hammering one burns the backoff window and trips account-lockout policy on
    exactly the stores this targets.

    `max_uploads: 1` so the count isolates *retries* from fan-out width: the
    upload pass stops polling at the first rejection, but whatever was already
    in flight has already been sent, so a wider run legitimately shows one
    attempt per concurrent slot.
    """
    source = publish_upstream(asset_server, [("0.5.8", "stable")])
    output = tmp_path / "public"
    spec = write_spec(
        tmp_path / "dist.yml",
        source,
        output,
        base_url=upload_store.url(),
        upload=True,
        retry_delays=[1, 1, 1, 1, 1],
        max_uploads=1,
    )
    dist_mirror.env["DIST_USER"] = "ci"
    dist_mirror.env["DIST_PASSWORD"] = "wrong"

    result = dist_mirror.run("dist", "sync", str(spec), check=False)

    assert result.returncode != 0
    assert len(upload_store.paths("PUT")) == 1, (
        f"a 401 must fail on the first attempt: {upload_store.paths('PUT')}"
    )


def test_two_releases_rendering_to_one_path_are_refused(dist_mirror, asset_server, tmp_path):
    """A layout that collapses distinct targets onto one path is refused.

    Overwriting would leave the manifest naming one file for two targets — a
    wrong-binary install with nothing in the tree to show for it.
    """
    source = publish_upstream(asset_server, [("0.5.8", "stable")])
    output = tmp_path / "public"
    # `{channel}` is identical across every row, so both targets collapse onto
    # the same rendered path.
    spec = write_spec(tmp_path / "dist.yml", source, output, layout="{channel}/ocx.tar.gz")

    result = dist_mirror.run("dist", "sync", str(spec), check=False)

    assert result.returncode != 0
    assert "already claimed" in result.stdout + result.stderr
    assert not (output / "dist.json").exists(), "a collision must publish no manifest"


# ---------------------------------------------------------------------------
# Destination probe + staging cleanup
# ---------------------------------------------------------------------------


def _upload_spec(tmp_path, source, output, **kw):
    spec = write_spec(tmp_path / "dist.yml", source, output, base_url=kw.pop("base_url"), upload=True, **kw)
    return spec


def test_an_archive_the_store_already_holds_is_never_downloaded(
    dist_mirror, asset_server, upload_store, tmp_path
):
    """The whole point of probing before fetching.

    A CI runner starts with an empty `output:`. Without the probe the second run
    re-downloads every archive from upstream only to discover the store already
    has it — for the real mirror that is ~1.9 GB pulled to upload nothing.
    """
    source = publish_upstream(asset_server, [("0.5.8", "stable")])
    dist_mirror.env["DIST_USER"] = "ci"
    dist_mirror.env["DIST_PASSWORD"] = "hunter2"

    first = _upload_spec(tmp_path, source, tmp_path / "run1", base_url=upload_store.url())
    dist_mirror.run("dist", "sync", str(first))

    # A fresh output directory: nothing local to reuse, so any skip must have
    # come from asking the destination.
    asset_server.requests.clear()
    second = write_spec(
        tmp_path / "dist2.yml", source, tmp_path / "run2", base_url=upload_store.url(), upload=True
    )
    result = dist_mirror.run("dist", "sync", str(second), "--format", "json")

    archive_gets = [r for r in asset_server.requests if r.startswith("GET /v0.5.8/")]
    assert archive_gets == [], f"no archive may be fetched when the store already holds it: {archive_gets}"
    report = json.loads(result.stdout)
    assert report["counters"]["skipped"] == report["counters"]["total"]
    assert report["counters"]["uploaded"] == 1, "only the rolling manifest is rewritten"


def test_a_destination_holding_different_bytes_is_re_uploaded(
    dist_mirror, asset_server, upload_store, tmp_path
):
    """The probe compares digests, not mere occupancy.

    A bare HEAD proves a path is taken, not that the right object is there. The
    old existence-only skip would have trusted a corrupted object for ever.
    """
    source = publish_upstream(asset_server, [("0.5.8", "stable")])
    dist_mirror.env["DIST_USER"] = "ci"
    dist_mirror.env["DIST_PASSWORD"] = "hunter2"
    spec = _upload_spec(tmp_path, source, tmp_path / "run1", base_url=upload_store.url())
    dist_mirror.run("dist", "sync", str(spec))

    corrupted = "/v0.5.8/ocx-x86_64-unknown-linux-gnu.tar.gz"
    upload_store.stored[corrupted] = b"not the archive anyone asked for"

    second = write_spec(
        tmp_path / "dist2.yml", source, tmp_path / "run2", base_url=upload_store.url(), upload=True
    )
    result = dist_mirror.run("dist", "sync", str(second), "--format", "json")

    assert upload_store.stored[corrupted] != b"not the archive anyone asked for", "the wrong object must be replaced"
    report = json.loads(result.stdout)
    assert report["counters"]["copied"] == 1, "exactly the corrupted row is re-fetched"


@pytest.mark.upload_store(omit_checksum=True)
def test_a_store_that_reports_no_checksum_still_skips_on_existence(
    dist_mirror, asset_server, upload_store, tmp_path
):
    """Plain WebDAV and S3-alikes answer a HEAD with no digest.

    The probe must degrade to the existence-only trust those stores always had,
    never to re-transferring the whole mirror on every run.
    """
    source = publish_upstream(asset_server, [("0.5.8", "stable")])
    dist_mirror.env["DIST_USER"] = "ci"
    dist_mirror.env["DIST_PASSWORD"] = "hunter2"
    spec = _upload_spec(tmp_path, source, tmp_path / "run1", base_url=upload_store.url())
    dist_mirror.run("dist", "sync", str(spec))

    asset_server.requests.clear()
    second = write_spec(
        tmp_path / "dist2.yml", source, tmp_path / "run2", base_url=upload_store.url(), upload=True
    )
    result = dist_mirror.run("dist", "sync", str(second), "--format", "json")

    assert [r for r in asset_server.requests if r.startswith("GET /v0.5.8/")] == []
    assert json.loads(result.stdout)["counters"]["failed"] == 0


def test_uploading_discards_each_staged_archive_but_emitting_keeps_them(
    dist_mirror, asset_server, upload_store, tmp_path
):
    """`retain_archives` auto, both halves.

    Without `upload:` the tree IS the deliverable. With it the store is, and the
    tree is staging — a full mirror is ~1.9 GB of archives, which is what fills
    a small runner's disk when every one of them is kept until the end.
    """
    source = publish_upstream(asset_server, [("0.5.8", "stable")])

    emitted = tmp_path / "emitted"
    dist_mirror.run("dist", "sync", str(write_spec(tmp_path / "emit.yml", source, emitted)))
    assert list(emitted.glob("v0.5.8/*")), "with no uploader the archives are the deliverable"

    staged = tmp_path / "staged"
    dist_mirror.env["DIST_USER"] = "ci"
    dist_mirror.env["DIST_PASSWORD"] = "hunter2"
    dist_mirror.run("dist", "sync", str(_upload_spec(tmp_path, source, staged, base_url=upload_store.url())))

    assert list(staged.glob("v0.5.8/*")) == [], "an uploaded archive must not stay on disk"
    assert (staged / "dist.json").is_file(), "the manifest is always kept — it is what the report names"
    assert list(staged.glob("dist/*.json")), "so is the snapshot"


def test_retain_archives_true_keeps_them_even_when_uploading(
    dist_mirror, asset_server, upload_store, tmp_path
):
    """The escape hatch for an operator who ships the tree *and* the store."""
    source = publish_upstream(asset_server, [("0.5.8", "stable")])
    output = tmp_path / "public"
    spec = tmp_path / "dist.yml"
    write_spec(spec, source, output, base_url=upload_store.url(), upload=True)
    spec.write_text(spec.read_text() + "retain_archives: true\n")
    dist_mirror.env["DIST_USER"] = "ci"
    dist_mirror.env["DIST_PASSWORD"] = "hunter2"

    dist_mirror.run("dist", "sync", str(spec))

    assert list(output.glob("v0.5.8/*")), "retain_archives: true must survive a successful upload"
