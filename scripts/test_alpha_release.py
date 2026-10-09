#!/usr/bin/env python3
"""Focused tests for the alpha release version guard."""

from __future__ import annotations

from contextlib import redirect_stderr
from io import StringIO
import os
from pathlib import Path
import re
import subprocess
import tempfile
import unittest
from unittest.mock import patch

import alpha_release
from alpha_release import (
    next_alpha_version,
    resolve_release_identity,
    validate_approved_allocation,
    verify_release_absent,
)


_SOURCE_SHA = "0123456789abcdef0123456789abcdef01234567"


class AlphaReleaseTests(unittest.TestCase):
    def test_stable_release_identity_forwards_calver_version_and_tag(self) -> None:
        self.assertEqual(
            resolve_release_identity(
                channel="stable",
                stable_version="2026.10.3",
                stable_tag="2026.10.3",
            ),
            ("stable", "2026.10.3", "2026.10.3"),
        )

        # Preserve the action's historical v-prefixed tag if it emits one.
        self.assertEqual(
            resolve_release_identity(
                channel="stable",
                stable_version="2026.10.3",
                stable_tag="v2026.10.3",
            ),
            ("stable", "2026.10.3", "v2026.10.3"),
        )

    def test_alpha_release_identity_keeps_full_prerelease_for_tag(self) -> None:
        self.assertEqual(
            resolve_release_identity(
                channel="alpha",
                stable_version="2026.10.3",
                stable_tag="2026.10.3",
                alpha_version="2026.10.3-alpha.4",
                alpha_tag="2026.10.3-alpha.4",
            ),
            ("alpha", "2026.10.3-alpha.4", "2026.10.3-alpha.4"),
        )

    def test_resolver_cli_emits_full_alpha_version_and_tag(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            output_path = Path(directory) / "github-output"
            environment = {
                "RELEASE_CHANNEL": "alpha",
                "STABLE_VERSION": "2026.10.3",
                "STABLE_TAG": "2026.10.3",
                "ALPHA_VERSION": "2026.10.3-alpha.4",
                "ALPHA_TAG": "2026.10.3-alpha.4",
                "GITHUB_OUTPUT": str(output_path),
            }
            with patch.dict(os.environ, environment, clear=False), patch.object(
                alpha_release.sys,
                "argv",
                ["alpha_release.py", "resolve-release"],
            ):
                self.assertEqual(alpha_release.main(), 0)
            self.assertEqual(
                output_path.read_text(encoding="utf-8"),
                "channel=alpha\n"
                "version=2026.10.3-alpha.4\n"
                "tag=2026.10.3-alpha.4\n",
            )

    def test_release_identity_rejects_mismatched_tag_or_channel(self) -> None:
        with self.assertRaisesRegex(ValueError, "tag does not match"):
            resolve_release_identity(
                channel="alpha",
                stable_version="2026.10.3",
                stable_tag="2026.10.3",
                alpha_version="2026.10.3-alpha.4",
                alpha_tag="2026.10.3-alpha.3",
            )
        with self.assertRaisesRegex(ValueError, "channel is invalid"):
            resolve_release_identity(
                channel="unknown",
                stable_version="2026.10.3",
                stable_tag="2026.10.3",
            )

    def test_workflow_routes_every_release_consumer_through_resolved_identity(self) -> None:
        workflow_path = (
            Path(__file__).resolve().parents[1]
            / ".github"
            / "workflows"
            / "release.yaml"
        )
        workflow = workflow_path.read_text(encoding="utf-8")

        def step_block(name: str) -> str:
            marker = f"      - name: {name}\n"
            start = workflow.find(marker)
            self.assertNotEqual(start, -1, f"missing workflow step: {name}")
            end_match = re.search(
                r"^      - name: ",
                workflow[start + len(marker) :],
                re.MULTILINE,
            )
            end = (
                start + len(marker) + end_match.start()
                if end_match
                else len(workflow)
            )
            return workflow[start:end]

        identity_step = step_block("Resolve release identity")
        self.assertIn("id: release_identity", identity_step)
        self.assertIn("STABLE_VERSION: ${{ steps.calver.outputs.version }}", identity_step)
        self.assertIn("STABLE_TAG: ${{ steps.calver.outputs.tag }}", identity_step)
        self.assertIn("ALPHA_VERSION: ${{ steps.alpha.outputs.version }}", identity_step)
        self.assertIn("ALPHA_TAG: ${{ steps.alpha.outputs.tag }}", identity_step)
        self.assertIn("python3 scripts/alpha_release.py resolve-release", identity_step)

        self.assertIn(
            "run: python3 -B scripts/test_alpha_release.py",
            step_block("Test alpha version allocator"),
        )
        ci_path = workflow_path.with_name("ci.yaml")
        ci_workflow = ci_path.read_text(encoding="utf-8")
        self.assertIn(
            "      - name: Test alpha release allocation and workflow contract\n"
            "        run: python3 -B scripts/test_alpha_release.py",
            ci_workflow,
        )

        expected_consumers = {
            "Refuse an existing alpha GitHub Release": ("TAG", "tag"),
            "Set package version": ("VERSION", "version"),
            "Create release-only commit": ("VERSION", "version"),
            "Push immutable CalVer tag": ("TAG", "tag"),
            "Dispatch dist release": ("TAG", "tag"),
            "Summary": ("CHANNEL", "channel"),
        }
        for name, (variable, output) in expected_consumers.items():
            with self.subTest(step=name):
                self.assertIn(
                    f"{variable}: ${{{{ steps.release_identity.outputs.{output} }}}}",
                    step_block(name),
                )

        dist_step = step_block("Dispatch dist release")
        self.assertIn('gh workflow run release.yml --ref "$TAG" -f "tag=$TAG"', dist_step)
        summary_step = step_block("Summary")
        self.assertIn(
            "VERSION: ${{ steps.release_identity.outputs.version }}", summary_step
        )
        self.assertIn("TAG: ${{ steps.release_identity.outputs.tag }}", summary_step)
        after_resolution = workflow[
            workflow.index("      - name: Resolve release identity\n") :
        ]
        direct_allocators = re.findall(
            r"steps\.(?:calver|alpha)\.outputs\.(?:version|tag)",
            after_resolution,
        )
        self.assertEqual(
            direct_allocators,
            [
                "steps.calver.outputs.version",
                "steps.calver.outputs.tag",
                "steps.alpha.outputs.version",
                "steps.alpha.outputs.tag",
            ],
            "only the resolver may read allocator outputs after its declaration",
        )

    def test_allocates_after_matching_tags_and_ignores_other_bases(self) -> None:
        self.assertEqual(
            next_alpha_version(
                "2026.10.2",
                [
                    "2026.10.2-alpha.1",
                    "2026.10.2-alpha.3",
                    "2026.10.1-alpha.99",
                    "2026.10.2",
                ],
            ),
            "2026.10.2-alpha.4",
        )

    def test_first_alpha_is_one(self) -> None:
        self.assertEqual(next_alpha_version("2026.8.0", []), "2026.8.0-alpha.1")

    def test_invalid_matching_tag_fails_closed(self) -> None:
        with self.assertRaisesRegex(ValueError, "invalid alpha tag"):
            next_alpha_version("2026.10.2", ["2026.10.2-alpha.01"])

    def test_invalid_base_version_fails_closed(self) -> None:
        with self.assertRaisesRegex(ValueError, "stable CalVer output"):
            next_alpha_version("2026.13.2", [])

    def test_approved_version_and_source_must_match_exact_allocation(self) -> None:
        self.assertEqual(
            validate_approved_allocation(
                base_version="2026.10.2",
                expected_alpha_version="2026.10.2-alpha.2",
                expected_source_sha=_SOURCE_SHA,
                source_sha=_SOURCE_SHA,
                tags=["2026.10.2-alpha.1"],
            ),
            "2026.10.2-alpha.2",
        )

    def test_retry_after_immutable_tag_cannot_reuse_approved_version(self) -> None:
        # A restarted allocator sees the tag from the earlier attempt and refuses
        # the old approval instead of assigning its content to a different tag.
        with self.assertRaisesRegex(ValueError, "next immutable candidate"):
            validate_approved_allocation(
                base_version="2026.10.2",
                expected_alpha_version="2026.10.2-alpha.1",
                expected_source_sha=_SOURCE_SHA,
                source_sha=_SOURCE_SHA,
                tags=["2026.10.2-alpha.1"],
            )

    def test_retry_mismatch_does_not_write_workflow_outputs(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            output_path = Path(directory) / "github-output"
            environment = {
                "BASE_VERSION": "2026.8.0",
                "EXPECTED_ALPHA_VERSION": "2026.8.0-alpha.1",
                "EXPECTED_SOURCE_SHA": _SOURCE_SHA,
                "SOURCE_SHA": _SOURCE_SHA,
                "GITHUB_OUTPUT": str(output_path),
            }
            git_tags = subprocess.CompletedProcess(
                args=["git", "tag", "--list"],
                returncode=0,
                stdout="2026.8.0-alpha.1\n",
                stderr="",
            )
            with patch.dict(os.environ, environment, clear=False), patch(
                "alpha_release.subprocess.run", return_value=git_tags
            ), redirect_stderr(StringIO()):
                self.assertEqual(alpha_release.main(), 1)
            self.assertFalse(output_path.exists())

    def test_source_mismatch_fails_before_version_output(self) -> None:
        with self.assertRaisesRegex(ValueError, "source SHA"):
            validate_approved_allocation(
                base_version="2026.10.2",
                expected_alpha_version="2026.10.2-alpha.1",
                expected_source_sha="f" * 40,
                source_sha=_SOURCE_SHA,
                tags=[],
            )

    def test_shell_metacharacters_are_rejected_before_gh_invocation(self) -> None:
        invoked = False

        def unexpected_run(*_args, **_kwargs):
            nonlocal invoked
            invoked = True

        with self.assertRaisesRegex(ValueError, "alpha tag is invalid"):
            verify_release_absent(
                repository="f4ah6o/temote-mcp",
                tag="2026.10.2-alpha.1;touch /tmp/not-run",
                run=unexpected_run,
            )
        self.assertFalse(invoked)

        with self.assertRaisesRegex(ValueError, "approved alpha version is invalid"):
            validate_approved_allocation(
                base_version="2026.10.2",
                expected_alpha_version="2026.10.2-alpha.1;touch /tmp/not-run",
                expected_source_sha=_SOURCE_SHA,
                source_sha=_SOURCE_SHA,
                tags=[],
            )

    def test_release_lookup_allows_only_confirmed_not_found(self) -> None:
        class Result:
            returncode = 1
            stdout = "HTTP/2.0 404 Not Found\n"

        verify_release_absent(
            repository="f4ah6o/temote-mcp",
            tag="2026.10.2-alpha.1",
            run=lambda *_args, **_kwargs: Result(),
        )

    def test_existing_or_unverifiable_release_blocks_allocation(self) -> None:
        class ExistingResult:
            returncode = 0
            stdout = "HTTP/2.0 200 OK\n"

        class UnverifiableResult:
            returncode = 1
            stdout = ""

        for result in (ExistingResult(), UnverifiableResult()):
            with self.subTest(result=result), self.assertRaises(ValueError):
                verify_release_absent(
                    repository="f4ah6o/temote-mcp",
                    tag="2026.10.2-alpha.1",
                    run=lambda *_args, result=result, **_kwargs: result,
                )


if __name__ == "__main__":
    unittest.main(verbosity=2)
