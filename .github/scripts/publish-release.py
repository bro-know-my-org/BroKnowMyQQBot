"""Publish checked assets through a recoverable draft without overwriting assets."""

import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import tempfile


def gh(*args):
    return subprocess.run(["gh", *args], check=True, capture_output=True, text=True).stdout


def api(path):
    return json.loads(gh("api", path))


def release_for_tag(repo, tag):
    # REST's /releases/tags endpoint only finds published releases. Resolve the
    # pending tag through GraphQL first, then read draft assets by numeric ID.
    owner, name = repo.split("/")
    query = ("query($owner:String!,$name:String!,$tag:String!){"
             "repository(owner:$owner,name:$name){release(tagName:$tag){databaseId}}}")
    data = json.loads(gh("api", "graphql", "-f", f"query={query}", "-f", f"owner={owner}",
                         "-f", f"name={name}", "-f", f"tag={tag}"))
    release = data["data"]["repository"]["release"]
    return api(f"repos/{repo}/releases/{release['databaseId']}") if release else None


def check_tag(repo, tag, revision):
    obj = api(f"repos/{repo}/git/ref/tags/{tag}")["object"]
    for _ in range(16):
        if obj["type"] == "commit":
            if obj["sha"] != revision:
                raise RuntimeError("remote tag no longer matches the verified build")
            return
        if obj["type"] != "tag":
            break
        obj = api(f"repos/{repo}/git/tags/{obj['sha']}")["object"]
    raise RuntimeError("release tag must resolve to a commit")


def digest(path):
    value = hashlib.sha256()
    with path.open("rb") as source:
        for block in iter(lambda: source.read(1024 * 1024), b""):
            value.update(block)
    return value.hexdigest()


def publish(directory, notes, repo, tag, revision):
    if not re.fullmatch(r"v[0-9]+\.[0-9]+\.[0-9]+", tag):
        raise ValueError("stable version tag required")
    if not re.fullmatch(r"[0-9a-f]{40}", revision):
        raise ValueError("full source revision required")
    archive = f"bkmqb-{tag}-x86_64-unknown-linux-gnu.tar.gz"
    expected = {archive, "SHA256SUMS"}
    if {p.name for p in directory.iterdir()} != expected:
        raise ValueError("release directory must contain exactly the archive and SHA256SUMS")
    checksums = (directory / "SHA256SUMS").read_text().split()
    if checksums != [digest(directory / archive), archive]:
        raise ValueError("archive checksum mismatch")
    check_tag(repo, tag, revision)
    release = release_for_tag(repo, tag)
    if release is None:
        gh("release", "create", tag, "--repo", repo, "--draft", "--verify-tag",
           "--target", revision, "--title", tag, "--generate-notes", "--notes-file", str(notes))
        release = release_for_tag(repo, tag)
    remote = {asset["name"]: asset for asset in release["assets"]}
    if len(remote) != len(release["assets"]) or set(remote) - expected:
        raise RuntimeError("release contains unexpected or duplicate assets")
    if not release["draft"] and set(remote) != expected:
        raise RuntimeError("published release is incomplete; refusing to change it")
    for name in sorted(expected):
        if name not in remote:
            gh("release", "upload", tag, str(directory / name), "--repo", repo)
    release = release_for_tag(repo, tag)
    remote = {asset["name"]: asset for asset in release["assets"]}
    if len(remote) != len(release["assets"]) or set(remote) != expected:
        raise RuntimeError("release assets changed during upload")
    with tempfile.TemporaryDirectory(prefix="bkmqb-release-check-") as temporary:
        for index, name in enumerate(sorted(expected)):
            downloaded = Path(temporary) / str(index)
            with downloaded.open("wb") as target:
                subprocess.run(["gh", "api", f"repos/{repo}/releases/assets/{remote[name]['id']}",
                                "-H", "Accept: application/octet-stream"], stdout=target, check=True)
            if digest(downloaded) != digest(directory / name):
                raise RuntimeError(f"remote asset differs from the verified build: {name}")
    check_tag(repo, tag, revision)
    if release["draft"]:
        gh("release", "edit", tag, "--repo", repo, "--draft=false")
    print(release["html_url"])


if __name__ == "__main__":
    publish(Path(sys.argv[1]), Path(sys.argv[2]), os.environ["GH_REPO"],
            os.environ["RELEASE_TAG"], os.environ["SOURCE_REVISION"])
