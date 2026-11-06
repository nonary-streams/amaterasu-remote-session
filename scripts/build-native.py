#!/usr/bin/env python3
"""Build the plugin against an explicit SDK without rewriting either checkout."""

import argparse
from copy import deepcopy
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import tomllib


DEPENDENCY_TABLES = ("dependencies", "dev-dependencies", "build-dependencies")
CORE_TEST_FEATURE = "core-integration-tests"


def read_manifest(path):
    with path.open("rb") as source:
        return tomllib.load(source)


def toml_value(value):
    """Cargo manifests use only these TOML values; reject unexpected types."""
    if isinstance(value, str):
        return json.dumps(value, ensure_ascii=False)
    if isinstance(value, bool):
        return "true" if value else "false"
    if isinstance(value, (int, float)):
        return repr(value)
    if isinstance(value, list):
        return "[" + ", ".join(toml_value(item) for item in value) + "]"
    if isinstance(value, dict):
        return "{ " + ", ".join(
            f"{json.dumps(key)} = {toml_value(item)}" for key, item in value.items()
        ) + " }"
    raise ValueError(f"unsupported Cargo manifest value: {type(value).__name__}")


def serialize_manifest(manifest):
    return "".join(
        f"{json.dumps(key)} = {toml_value(value)}\n"
        for key, value in manifest.items()
    )


def package_identity(path):
    return read_manifest(path / "Cargo.toml").get("package", {}).get("name")


def resolve_dependency(name, dependency, repository, sdk_root, workspace):
    if not isinstance(dependency, dict):
        return dependency
    dependency = deepcopy(dependency)
    from_workspace = dependency.pop("workspace", False)
    if from_workspace:
        inherited = workspace.get("dependencies", {}).get(name)
        if inherited is None:
            raise ValueError(f"SDK does not declare workspace dependency {name}")
        inherited = {"version": inherited} if isinstance(inherited, str) else deepcopy(inherited)
        inherited_features = inherited.pop("features", [])
        extra_features = dependency.pop("features", [])
        if inherited_features or extra_features:
            inherited["features"] = list(dict.fromkeys(inherited_features + extra_features))
        inherited.update(dependency)
        dependency = inherited
        path_root = sdk_root
    else:
        path_root = repository
    if "path" not in dependency:
        return dependency
    dependency_path = (path_root / dependency["path"]).resolve()
    expected_name = dependency.get("package", name)
    if not from_workspace and not dependency_path.is_relative_to(repository):
        # Match SDK identity, rather than replacing a substring in a path. This
        # also handles renamed dependencies and nested SDK checkout locations.
        matches = []
        for sdk_name, sdk_dependency in workspace.get("dependencies", {}).items():
            if not isinstance(sdk_dependency, dict) or "path" not in sdk_dependency:
                continue
            if sdk_dependency.get("package", sdk_name) == expected_name:
                matches.append((sdk_root / sdk_dependency["path"]).resolve())
        if len(matches) != 1:
            raise ValueError(f"no unique SDK path dependency for {expected_name}")
        dependency_path = matches[0]
    if not (dependency_path / "Cargo.toml").is_file():
        raise ValueError(f"dependency manifest missing: {dependency_path}")
    if package_identity(dependency_path) != expected_name:
        raise ValueError(f"dependency {name} has the wrong package identity")
    dependency["path"] = str(dependency_path)
    return dependency


def omit_core_integration_tests(manifest):
    """Core provider regressions stay in the core workspace, outside the SDK."""
    metadata = manifest.get("package", {}).get("metadata", {})
    standalone = metadata.get("amaterasu", {}).get("standalone", {})
    core_dependencies = standalone.get("core-test-dependencies", [])
    if not isinstance(core_dependencies, list) or any(
        not isinstance(name, str) or not name for name in core_dependencies
    ) or len(set(core_dependencies)) != len(core_dependencies):
        raise ValueError("core-test-dependencies must list distinct dependency names")
    if not core_dependencies:
        return
    features = manifest.get("features", {})
    if features.get(CORE_TEST_FEATURE) != []:
        raise ValueError(f"core test dependencies require the empty {CORE_TEST_FEATURE} feature")
    removed = set()
    for table in [manifest, *manifest.get("target", {}).values()]:
        dependencies = table.get("dev-dependencies", {})
        for name in core_dependencies:
            if name in dependencies:
                del dependencies[name]
                removed.add(name)
    if removed != set(core_dependencies):
        raise ValueError("core-test-dependencies must name declared dev-dependencies")
    features["default"] = [
        name for name in features.get("default", []) if name != CORE_TEST_FEATURE
    ]


def standalone_manifest(repository, sdk_root):
    manifest = read_manifest(repository / "Cargo.toml")
    workspace = read_manifest(sdk_root / "Cargo.toml").get("workspace", {})
    package = manifest.get("package")
    if not isinstance(package, dict) or package.get("name") != "amaterasu-remote-session":
        raise ValueError("plugin manifest must declare amaterasu-remote-session")
    omit_core_integration_tests(manifest)
    for key, value in list(package.items()):
        if isinstance(value, dict) and value.get("workspace") is True:
            inherited = workspace.get("package", {}).get(key)
            if inherited is None:
                raise ValueError(f"SDK does not declare workspace package field {key}")
            package[key] = deepcopy(inherited)
    package.pop("workspace", None)
    for table in [manifest, *manifest.get("target", {}).values()]:
        for kind in DEPENDENCY_TABLES:
            dependencies = table.get(kind, {})
            for name, dependency in list(dependencies.items()):
                dependencies[name] = resolve_dependency(
                    name, dependency, repository, sdk_root, workspace
                )
    if manifest.get("lints", {}).get("workspace") is True:
        manifest["lints"] = deepcopy(workspace.get("lints", {}))
    manifest.pop("workspace", None)
    library = manifest.setdefault("lib", {})
    library["path"] = str((repository / library.get("path", "src/lib.rs")).resolve())
    build = package.get("build")
    if isinstance(build, str):
        package["build"] = str((repository / build).resolve())
    elif build is not False and (repository / "build.rs").is_file():
        package["build"] = str(repository / "build.rs")
    for field in ("readme", "license-file"):
        if isinstance(package.get(field), str):
            package[field] = str((repository / package[field]).resolve())
    # Cargo discovers these relative to the generated manifest, so preserve the
    # original source files explicitly when the plugin ships extra test targets.
    for kind, directory in (("test", "tests"), ("bench", "benches"), ("example", "examples")):
        targets = manifest.setdefault(kind, [])
        explicit_names = {target["name"] for target in targets}
        for target in targets:
            default = f"{directory}/{target['name']}.rs"
            target["path"] = str((repository / target.get("path", default)).resolve())
        for path in sorted((repository / directory).glob("*.rs")):
            if path.stem not in explicit_names:
                targets.append({"name": path.stem, "path": str(path)})
        if not targets:
            manifest.pop(kind)
    return manifest


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("sdk_root", type=Path, help="Amaterasu SDK checkout")
    operation = parser.add_mutually_exclusive_group()
    operation.add_argument("--test", action="store_true", help="run native plugin tests")
    operation.add_argument("--clippy", action="store_true", help="check all targets with warnings denied")
    operation.add_argument("--fmt", action="store_true", help="check Rust formatting without modifying files")
    parser.add_argument("--release", action="store_true", help="use Cargo's release profile")
    parser.add_argument("--offline", action="store_true", help="use only cached Cargo dependencies")
    args = parser.parse_args()
    if args.fmt and args.release:
        parser.error("--release does not apply to formatting checks")
    if not os.environ.get("CARGO_TARGET_DIR"):
        parser.error("set CARGO_TARGET_DIR to the intended build volume")
    repository = Path(__file__).resolve().parents[1]
    try:
        sdk_root = args.sdk_root.resolve(strict=True)
        if not (sdk_root / "crates/module-sdk/Cargo.toml").is_file():
            raise ValueError(f"SDK source missing: {sdk_root / 'crates/module-sdk'}")
        manifest = standalone_manifest(repository, sdk_root)
        target = Path(os.environ["CARGO_TARGET_DIR"]).resolve()
        target.mkdir(parents=True, exist_ok=True)
    except (OSError, ValueError, tomllib.TOMLDecodeError) as error:
        parser.error(str(error))
    environment = os.environ.copy()
    environment["CARGO_TARGET_DIR"] = str(target)
    with tempfile.TemporaryDirectory(prefix="amaterasu-remote-session-manifest-", dir=target) as temporary:
        manifest_path = Path(temporary) / "Cargo.toml"
        # The temporary crate is its own workspace, keeping Cargo's generated
        # lockfile and metadata off both source checkouts.
        manifest["workspace"] = {"resolver": "2"}
        manifest_path.write_text(serialize_manifest(manifest), encoding="utf-8")
        action = "test" if args.test else "clippy" if args.clippy else "fmt" if args.fmt else "build"
        command = ["cargo", action, "--manifest-path", str(manifest_path)]
        if args.release:
            command.append("--release")
        if args.offline and not args.fmt:
            command.append("--offline")
        if args.clippy:
            command.extend(["--all-targets", "--", "-D", "warnings"])
        elif args.fmt:
            command.extend(["--", "--check"])
        result = subprocess.run(command, env=environment, check=False)
        return result.returncode


if __name__ == "__main__":
    sys.exit(main())
