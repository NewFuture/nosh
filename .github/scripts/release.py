"""Version, package, and publish nosh's native CPU releases."""

import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import tarfile
import tomllib
import zipfile


NUMBER = r"(?:0|[1-9][0-9]*)"
IDENTIFIER = rf"(?:{NUMBER}|[0-9]*[A-Za-z-][0-9A-Za-z-]*)"
SEMVER = re.compile(
    rf"{NUMBER}\.{NUMBER}\.{NUMBER}"
    rf"(?:-(?P<prerelease>{IDENTIFIER}(?:\.{IDENTIFIER})*))?"
    r"(?:\+[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*)?"
)


def parse_version(version: str) -> re.Match[str]:
    match = SEMVER.fullmatch(version)
    if match is None:
        raise ValueError("Expected a full SemVer version without a leading v.")
    return match


def update_manifest(manifest: Path, version: str) -> None:
    parse_version(version)
    text = manifest.read_text(encoding="utf-8")
    if tomllib.loads(text)["workspace"]["package"]["version"] == version:
        return
    section = re.search(r"(?ms)^\[workspace\.package\][^\n]*\n.*?(?=^\[|\Z)", text)
    if section is None:
        raise ValueError("Missing [workspace.package] section.")
    updated, count = re.subn(
        r'(?m)^([ \t]*version[ \t]*=[ \t]*)"[^"\n]*"([ \t]*(?:#.*)?)$',
        lambda match: f"{match[1]}{json.dumps(version)}{match[2]}",
        section[0],
    )
    if count != 1:
        raise ValueError("Expected exactly one workspace package version.")
    text = text[: section.start()] + updated + text[section.end() :]
    if tomllib.loads(text)["workspace"]["package"]["version"] != version:
        raise ValueError("Workspace version was not updated.")
    manifest.write_text(text, encoding="utf-8", newline="\n")


def ensure_tag(version: str, commit: str) -> None:
    repo = os.environ["GITHUB_REPOSITORY"]
    tag = f"v{version}"
    refs = json.loads(
        subprocess.check_output(
            ["gh", "api", f"repos/{repo}/git/matching-refs/tags/{tag}"], text=True
        )
    )
    for ref in refs:
        if ref["ref"] == f"refs/tags/{tag}" and (
            ref["object"]["type"] != "commit" or ref["object"]["sha"] != commit
        ):
            raise ValueError(f"Tag {tag} does not point directly to commit {commit}.")


def commit_version(root: Path, version: str) -> None:
    parse_version(version)
    if os.environ["GITHUB_REF_TYPE"] != "branch":
        raise ValueError("Run the release workflow on a branch, not a tag.")
    branch = os.environ["GITHUB_REF_NAME"]
    output_file = Path(os.environ["GITHUB_OUTPUT"])
    update_manifest(root / "Cargo.toml", version)
    subprocess.run(["cargo", "update", "--workspace"], cwd=root, check=True)
    subprocess.run(["git", "add", "--", "Cargo.toml", "Cargo.lock"], cwd=root, check=True)
    diff = subprocess.run(
        ["git", "diff", "--cached", "--quiet", "--", "Cargo.toml", "Cargo.lock"],
        cwd=root,
    )
    if diff.returncode == 1:
        subprocess.run(
            ["git", "config", "user.name", "github-actions[bot]"], cwd=root, check=True
        )
        subprocess.run(
            [
                "git", "config", "user.email",
                "41898282+github-actions[bot]@users.noreply.github.com",
            ],
            cwd=root,
            check=True,
        )
        subprocess.run(
            [
                "git", "commit", "-m", f"chore: release v{version}",
                "--", "Cargo.toml", "Cargo.lock",
            ],
            cwd=root,
            check=True,
        )
    else:
        diff.check_returncode()
        print(f"Version is already {version}; reusing the current commit.")
    commit = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=root, text=True).strip()
    ensure_tag(version, commit)
    if diff.returncode == 1:
        subprocess.run(
            ["git", "push", "origin", f"HEAD:refs/heads/{branch}"], cwd=root, check=True
        )
    with output_file.open("a", encoding="utf-8", newline="\n") as output:
        output.write(f"version={version}\ncommit={commit}\n")


def checksum(archive: Path) -> str:
    with archive.open("rb") as stream:
        digest = hashlib.file_digest(stream, "sha256").hexdigest()
    return f"{digest}  {archive.name}\n"


def package(root: Path, assets: Path, version: str, target: str) -> None:
    parse_version(version)
    windows = target.endswith("-windows-msvc")
    binary = root / "target" / target / "release" / ("nosh.exe" if windows else "nosh")
    actual = subprocess.check_output([str(binary), "--version"], text=True, timeout=30)
    if actual.splitlines() != [f"nosh {version}"]:
        raise ValueError(f"Unexpected executable version: {actual!r}")
    assets.mkdir(parents=True, exist_ok=True)
    extension = "zip" if windows else "tar.gz"
    archive = assets / f"nosh-{version}-{target}.{extension}"
    files = [binary, root / "LICENSE"]
    if windows:
        with zipfile.ZipFile(archive, "w", compression=zipfile.ZIP_DEFLATED) as output:
            for file in files:
                output.write(file, arcname=file.name)
    else:
        with tarfile.open(archive, "w:gz") as output:
            for file in files:
                member = tarfile.TarInfo(file.name)
                member.size = file.stat().st_size
                member.mode = 0o755 if file == binary else 0o644
                with file.open("rb") as stream:
                    output.addfile(member, stream)
    archive.with_name(archive.name + ".sha256").write_text(
        checksum(archive), encoding="utf-8", newline="\n"
    )


def publish(assets: Path, version: str, commit: str, count: int) -> None:
    match = parse_version(version)
    archives = sorted(
        file
        for file in assets.glob(f"nosh-{version}-*")
        if file.name.endswith((".zip", ".tar.gz"))
    )
    if count < 1 or len(archives) != count:
        raise ValueError(f"Expected {count} platform archives, found {len(archives)}.")
    checksums = []
    for archive in archives:
        expected = checksum(archive)
        if archive.with_name(archive.name + ".sha256").read_text(encoding="utf-8") != expected:
            raise ValueError(f"Checksum mismatch: {archive.name}")
        checksums.append(expected)
    sums = assets / "SHA256SUMS"
    sums.write_text("".join(checksums), encoding="utf-8", newline="\n")
    repo = os.environ["GITHUB_REPOSITORY"]
    tag = f"v{version}"
    ensure_tag(version, commit)
    # gh uploads every asset to a draft before publishing an immutable release.
    subprocess.run(
        [
            "gh", "release", "create", tag, *map(str, archives), str(sums),
            "--repo", repo, "--target", commit, "--generate-notes",
            f"--prerelease={str(match['prerelease'] is not None).lower()}",
        ],
        check=True,
    )


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    version = commands.add_parser("version")
    version.add_argument("version")
    archive = commands.add_parser("package")
    archive.add_argument("version")
    archive.add_argument("target")
    release = commands.add_parser("publish")
    release.add_argument("version")
    release.add_argument("commit")
    release.add_argument("count", type=int)
    args = parser.parse_args()
    if args.command == "version":
        commit_version(Path.cwd(), args.version)
    else:
        assets = Path(os.environ["RUNNER_TEMP"]) / "nosh-release"
        if args.command == "package":
            package(Path.cwd(), assets, args.version, args.target)
        else:
            publish(assets, args.version, args.commit, args.count)


if __name__ == "__main__":
    main()
