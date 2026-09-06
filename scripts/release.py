"""Prepare coordinated workspace versions and require CI on the selected commit."""

import argparse
import json
import os
from pathlib import Path
import re
import subprocess
import time
import tomllib

CHECKS = {"Check Formatting", "Lint", "Test"}


def next_version(version, bump):
    if not re.fullmatch(r"\d+\.\d+\.\d+", version):
        raise ValueError(f"expected a release version, got {version!r}")
    parts = [int(part) for part in version.split(".")]
    index = {"major": 0, "minor": 1, "patch": 2}[bump]
    parts[index] += 1
    parts[index + 1:] = [0] * (2 - index)
    return ".".join(map(str, parts))


def rewrite_section(source, section, replace):
    pattern = rf"(?ms)(^\[{re.escape(section)}\]\n)(.*?)(?=^\[|\Z)"
    updated, count = re.subn(pattern, lambda match: match[1] + replace(match[2]), source)
    if count != 1:
        raise ValueError(f"expected one [{section}] section")
    return updated


def bump_manifest(source, bump):
    workspace = tomllib.loads(source)["workspace"]
    old = workspace["package"]["version"]
    new = next_version(old, bump)
    source = rewrite_section(source, "workspace.package", lambda body: body.replace(f'version = "{old}"', f'version = "{new}"'))
    for name, dependency in workspace["dependencies"].items():
        if isinstance(dependency, dict) and "path" in dependency:
            source = bump_dependency(source, name, dependency, old, new)
    if tomllib.loads(source)["workspace"]["package"]["version"] != new:
        raise ValueError("workspace version was not updated")
    return source, old, new


def bump_dependency(source, name, dependency, old, new):
    if dependency.get("version") != f"={old}":
        raise ValueError(f"{name} must use the coordinated version ={old}")
    pattern = rf'(?m)(^{re.escape(name)}\s*=\s*\{{[^\n]*\bversion\s*=\s*")={re.escape(old)}(")'
    source = rewrite_section(source, "workspace.dependencies", lambda body: re.sub(pattern, rf'\g<1>={new}\2', body))
    if tomllib.loads(source)["workspace"]["dependencies"][name]["version"] != f"={new}":
        raise ValueError(f"{name} dependency version was not updated")
    return source


def output(*command):
    return subprocess.check_output(command, text=True).strip()


def prepare(bump):
    path = Path("Cargo.toml")
    updated, old, new = bump_manifest(path.read_text(), bump)
    metadata = json.loads(output("cargo", "metadata", "--locked", "--no-deps", "--format-version", "1"))
    members = [package for package in metadata["packages"] if package["id"] in metadata["workspace_members"]]
    if any(package["version"] != old for package in members):
        raise ValueError("workspace package versions differ")
    path.write_text(updated)
    subprocess.run(["cargo", "update", "--workspace"], check=True)
    values = f"old_version={old}\nnew_version={new}\ntag_name=v{new}\n"
    if destination := os.environ.get("GITHUB_OUTPUT"):
        with open(destination, "a") as stream:
            stream.write(values)
    print(values, end="")


def checks_passed(checks):
    latest = {}
    for check in sorted(checks, key=lambda check: check["id"]):
        if check.get("app", {}).get("slug") == "github-actions":
            latest[check["name"]] = check
    selected = [latest[name] for name in CHECKS if name in latest]
    failed = [check["name"] for check in selected if check["status"] == "completed" and check["conclusion"] != "success"]
    if failed:
        raise ValueError(f"required CI failed: {', '.join(sorted(failed))}")
    return len(selected) == len(CHECKS) and all(check["conclusion"] == "success" and check["status"] == "completed" for check in selected)


def check_ci():
    repository = os.environ["GITHUB_REPOSITORY"]
    commit = output("git", "rev-parse", "HEAD")
    endpoint = f"repos/{repository}/commits/{commit}/check-runs?per_page=100"
    for _ in range(30):
        pages = json.loads(output("gh", "api", endpoint, "--paginate", "--slurp"))
        if checks_passed([check for page in pages for check in page["check_runs"]]):
            return
        print(f"Waiting for required CI on {commit}", flush=True)
        time.sleep(30)
    raise ValueError(f"required CI did not pass on {commit}")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    commands.add_parser("prepare").add_argument("bump", choices=["patch", "minor", "major"])
    commands.add_parser("check-ci")
    arguments = parser.parse_args()
    if arguments.command == "prepare":
        prepare(arguments.bump)
    else:
        check_ci()


if __name__ == "__main__":
    main()
