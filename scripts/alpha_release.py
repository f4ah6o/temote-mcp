#!/usr/bin/env python3
"""Allocate an approved, immutable CalVer alpha identifier for release.yaml."""

from __future__ import annotations

import os
import re
import subprocess
import sys
from pathlib import Path
from typing import Iterable


_BASE_VERSION = re.compile(r"[0-9]{4}\.(?:[1-9]|1[0-2])\.(?:0|[1-9][0-9]*)\Z")
_STABLE_TAG = re.compile(r"v?[0-9]{4}\.(?:[1-9]|1[0-2])\.(?:0|[1-9][0-9]*)\Z")
_ALPHA_VERSION = re.compile(
    r"[0-9]{4}\.(?:[1-9]|1[0-2])\.(?:0|[1-9][0-9]*)-alpha\.[1-9][0-9]*\Z"
)
_SOURCE_SHA = re.compile(r"[0-9a-f]{40}\Z")
_RELEASE_TAG = re.compile(
    r"[0-9]{4}\.(?:[1-9]|1[0-2])\.(?:0|[1-9][0-9]*)-alpha\.[1-9][0-9]*\Z"
)
_HTTP_STATUS = re.compile(r"^HTTP/[0-9.]+\s+([0-9]{3})\b", re.MULTILINE | re.IGNORECASE)


def next_alpha_version(base_version: str, tags: Iterable[str]) -> str:
    if not _BASE_VERSION.fullmatch(base_version):
        raise ValueError("stable CalVer output is invalid")

    prefix = f"{base_version}-alpha."
    suffixes: list[int] = []
    for tag in tags:
        if not tag.startswith(prefix):
            continue
        suffix = tag[len(prefix) :]
        if not re.fullmatch(r"[1-9][0-9]*", suffix):
            raise ValueError("an invalid alpha tag exists for this CalVer base")
        suffixes.append(int(suffix))

    return f"{prefix}{max(suffixes, default=0) + 1}"


def validate_approved_allocation(
    *,
    base_version: str,
    expected_alpha_version: str,
    expected_source_sha: str,
    source_sha: str,
    tags: Iterable[str],
) -> str:
    if not _SOURCE_SHA.fullmatch(expected_source_sha) or not _SOURCE_SHA.fullmatch(
        source_sha
    ):
        raise ValueError("source SHA must be a full lowercase 40-character commit SHA")
    if expected_source_sha != source_sha:
        raise ValueError("approved source SHA does not match this workflow run")
    if not _ALPHA_VERSION.fullmatch(expected_alpha_version):
        raise ValueError("approved alpha version is invalid")

    candidate = next_alpha_version(base_version, tags)
    if candidate != expected_alpha_version:
        raise ValueError(
            f"approved alpha version does not match next immutable candidate {candidate}"
        )
    return candidate


def resolve_release_identity(
    *,
    channel: str,
    stable_version: str,
    stable_tag: str,
    alpha_version: str = "",
    alpha_tag: str = "",
) -> tuple[str, str, str]:
    """Resolve the one version/tag pair consumed by every release phase.

    The stable path forwards the CalVer action's exact version and tag, allowing
    its historical optional ``v`` tag prefix without changing stable releases.
    Alpha versions use the full prerelease identifier as both package version
    and immutable Git tag.
    """
    if channel == "stable":
        if not _BASE_VERSION.fullmatch(stable_version):
            raise ValueError("stable CalVer output is invalid")
        if not _STABLE_TAG.fullmatch(stable_tag) or stable_tag.removeprefix("v") != stable_version:
            raise ValueError("stable CalVer tag does not match its version")
        return channel, stable_version, stable_tag

    if channel == "alpha":
        if not _ALPHA_VERSION.fullmatch(alpha_version):
            raise ValueError("approved alpha version is invalid")
        if alpha_tag != alpha_version:
            raise ValueError("approved alpha tag does not match its version")
        return channel, alpha_version, alpha_tag

    raise ValueError("release channel is invalid")


def verify_release_absent(
    *, repository: str, tag: str, run=subprocess.run
) -> None:
    if not re.fullmatch(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+", repository):
        raise ValueError("GitHub repository identity is invalid")
    if not _RELEASE_TAG.fullmatch(tag):
        raise ValueError("alpha tag is invalid")

    result = run(
        [
            "gh",
            "api",
            "--include",
            "--silent",
            f"repos/{repository}/releases/tags/{tag}",
        ],
        check=False,
        capture_output=True,
        text=True,
    )
    statuses = [int(match.group(1)) for match in _HTTP_STATUS.finditer(result.stdout)]
    if not statuses:
        raise ValueError("could not verify that the alpha GitHub Release is absent")

    status = statuses[-1]
    if status == 404 and result.returncode != 0:
        return
    if 200 <= status < 300 and result.returncode == 0:
        raise ValueError("a GitHub Release already exists for this alpha tag")
    raise ValueError("could not verify that the alpha GitHub Release is absent")


def main() -> int:
    try:
        if sys.argv[1:] == ["check-release"]:
            verify_release_absent(
                repository=os.environ["GITHUB_REPOSITORY"],
                tag=os.environ["TAG"],
            )
            return 0

        if sys.argv[1:] == ["resolve-release"]:
            channel, version, tag = resolve_release_identity(
                channel=os.environ["RELEASE_CHANNEL"],
                stable_version=os.environ["STABLE_VERSION"],
                stable_tag=os.environ["STABLE_TAG"],
                alpha_version=os.environ.get("ALPHA_VERSION", ""),
                alpha_tag=os.environ.get("ALPHA_TAG", ""),
            )
            output_path = Path(os.environ["GITHUB_OUTPUT"])
            with output_path.open("a", encoding="utf-8") as output:
                output.write(f"channel={channel}\n")
                output.write(f"version={version}\n")
                output.write(f"tag={tag}\n")
            return 0

        base_version = os.environ["BASE_VERSION"]
        expected_alpha_version = os.environ["EXPECTED_ALPHA_VERSION"]
        expected_source_sha = os.environ["EXPECTED_SOURCE_SHA"]
        source_sha = os.environ["SOURCE_SHA"]
        output_path = Path(os.environ["GITHUB_OUTPUT"])

        fetched = subprocess.run(
            ["git", "tag", "--list"], check=True, capture_output=True, text=True
        )
        version = validate_approved_allocation(
            base_version=base_version,
            expected_alpha_version=expected_alpha_version,
            expected_source_sha=expected_source_sha,
            source_sha=source_sha,
            tags=fetched.stdout.splitlines(),
        )
        with output_path.open("a", encoding="utf-8") as output:
            output.write(f"version={version}\n")
            output.write(f"tag={version}\n")
    except (KeyError, OSError, subprocess.CalledProcessError, ValueError) as error:
        print(f"alpha allocation failed: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
