"""Bounded command parsing and approval policies for owned fixtures."""

from __future__ import annotations

from pathlib import Path
import re
import shlex

from . import fixtures
from .fixtures import protected_files

APPROVAL_CHECKS = {
    "rename": {"rename"}, "cwd": {"cwd"},
    "rust-build": {"rust-build", "build-failure"}, "rust-test": {"rust-test"},
    "rust-clean": {"rust-clean"}, "node-build": {"node-build"}, "node-test": {"node-test"},
    "python-test": {"python-test", "test-failure"}, "git-commit": {"git-commit"},
}

PROJECT_POLICIES = APPROVAL_CHECKS.keys() - {"rename", "cwd"}

def shell_parts(command: str) -> list[str]:
    if any(ch in command for ch in "$`<>\\\0"):
        raise ValueError("shell expansion/redirection is outside the allowed subset")
    lex = shlex.shlex(command, posix=True, punctuation_chars=";&|")
    lex.whitespace_split = True
    lex.commenters = ""
    return list(lex)


def command_groups(command: str, root: Path, tools: dict | None = None) -> list[list[str]]:
    cleaned = []
    quote = None
    i = 0
    while i < len(command):
        char = command[i]
        if quote is not None:
            if char == quote:
                quote = None
        elif char in ("'", '"'):
            quote = char
        elif (command.startswith("2>&1", i) and i > 0 and command[i - 1] in " \t\n"
              and re.match(r"[ \t\n]*(?:&&|;|$)", command[i + 4:])):
            cleaned.append(" ")
            i += 4
            continue
        cleaned.append(char)
        i += 1
    parts = shell_parts("".join(cleaned))
    if parts[:1] == ["cd"]:
        separator = parts.index("&&") if "&&" in parts else -1
        directory = parts[1:separator]
        if directory[:1] == ["--"]:
            directory = directory[1:]
        if separator < 0 or len(directory) != 1 or (root / directory[0]).resolve() != root:
            raise ValueError("only an initial cd to the fixture root is supported")
        parts = parts[separator + 1:]
    groups, group = [], []
    for word in parts:
        if word in ("&&", ";"):
            if not group:
                raise ValueError("empty command")
            groups.append(group)
            group = []
        elif word in ("&", "|", "||") or re.fullmatch(r"[;&|]+", word):
            raise ValueError("unsupported command operator")
        else:
            group.append(word)
    if group:
        groups.append(group)
    elif parts and parts[-1] != ";":
        raise ValueError("incomplete command")
    for group in groups:
        if group[:1] == ["env"]:
            group.pop(0)
        while group and group[0] in ("CARGO_NET_OFFLINE=true", "CARGO_INCREMENTAL=0", "CARGO_BUILD_JOBS=1"):
            group.pop(0)
        if not group:
            raise ValueError("missing command")
        if "/" in group[0]:
            executable = (root / group[0]).resolve()
            matches = [name for name, info in (tools or {}).items() if executable == Path(info["path"])]
            if len(matches) != 1:
                raise ValueError("executable is not a recorded tool")
            group[0] = matches[0]
    return groups


def project_action(parts: list[str], root: Path) -> str | None:
    if parts[:1] == ["cargo"] and len(parts) >= 2:
        verb, args = parts[1], parts[2:]
        if "--manifest-path" in args:
            index = args.index("--manifest-path")
            if index + 1 >= len(args) or (root / args[index + 1]).resolve() != root / "Cargo.toml":
                return None
            args = args[:index] + args[index + 2:]
        common = {"--offline", "--locked", "--frozen", "--quiet", "-q", "--verbose", "-v"}
        if verb in ("build", "test") and "--" in args:
            split = args.index("--")
            if verb != "test" or any(a not in ("--nocapture", "--test-threads=1") for a in args[split + 1:]):
                return None
            args = args[:split]
        flags = common | ({"--release", "--workspace", "--all-targets"} if verb != "clean" else set())
        if verb in ("build", "test", "clean") and all(arg in flags for arg in args):
            return "rust-" + verb
    if parts[:1] == ["npm"]:
        args = [p for p in parts[1:] if p not in ("--offline", "--silent", "-s", "--no-audit", "--no-fund")]
        if args == ["run", "build"]:
            return "node-build"
        if args in (["test"], ["run", "test"]):
            return "node-test"
    if parts[:2] == ["node", "--test"]:
        if all(p in ("--test-reporter=tap", "test/math.test.js") for p in parts[2:]):
            return "node-test"
    if len(parts) == 2 and parts[0] == "node" and (root / parts[1]).resolve() == root / "build.js":
        return "node-build"
    if parts[:1] == ["python3"]:
        args = [p for p in parts[1:] if p != "-B"]
        if args[:2] == ["-m", "unittest"]:
            rest = [p for p in args[2:] if p not in ("-v", "-q")]
            if rest in ([], ["discover"], ["discover", "-s", "tests"],
                        ["discover", "-s", "tests", "-p", "test*.py"]):
                return "python-test"
    if parts[:2] == ["git", "add"] and len(parts) > 2:
        if all(p in (".", "./", "--", "-A", "--all", "-u", "--update", "maths.py", "README.md") for p in parts[2:]):
            return "git-add"
    if parts[:2] == ["git", "commit"]:
        args = parts[2:]
        messages = 0
        i = 0
        while i < len(args):
            if args[i] in ("-a", "--all", "--quiet", "-q"):
                i += 1
            elif args[i] in ("-m", "--message", "-am", "-ma") and i + 1 < len(args) and args[i + 1].strip():
                messages += 1
                i += 2
            else:
                return None
        if messages:
            return "git-commit"
    if parts[:1] == ["rm"]:
        flags = [p for p in parts[1:] if p.startswith("-")]
        paths = [p for p in parts[1:] if not p.startswith("-")]
        if (all(p in ("-r", "-f", "-rf", "-fr", "--recursive", "--force", "--") for p in flags)
                and any(p in ("-r", "-rf", "-fr", "--recursive") for p in flags)
                and len(paths) == 1 and (root / paths[0]).resolve() == root / "target"
                and not (root / "target").is_symlink()):
            return "rust-clean"
    return None


def project_actions(policy: str, command: str, root: Path, facts: dict) -> set[str]:
    if facts.get("project") not in fixtures.PROJECT_FIXTURES:
        return set()
    before = protected_files(facts["before"], policy)
    try:
        groups = command_groups(command, root, facts.get("tools"))
    except ValueError:
        return set()
    actions = set()
    for parts in groups:
        action = project_action(parts, root)
        if action:
            actions.add(action)
        elif parts[:1] == ["cat"]:
            args = parts[1:]
            while args and args[0] in ("--", "-n", "-b", "-s"):
                args = args[1:]
            sources = {(root / name).resolve() for name in before}
            if not args or any((root / name).resolve() not in sources for name in args):
                return set()
        elif parts[:1] == ["pwd"] and all(p in ("-P", "-L", "--") for p in parts[1:]):
            continue
        elif parts[:1] == ["ls"] and all(p in (".", "--", "-l", "-a", "-la", "-al", "-lh", "-lah") for p in parts[1:]):
            continue
        elif parts[:2] == ["git", "status"] and all(p in ("--short", "-s", "-sb", "--porcelain", "--porcelain=v1") for p in parts[2:]):
            continue
        else:
            return set()
    allowed = {"git-add", "git-commit"} if policy == "git-commit" else {policy}
    return actions if actions <= allowed else set()


def allow_project_approval(policy: str, command: str, root: Path, facts: dict) -> bool:
    return bool(project_actions(policy, command, root, facts)) and (
        protected_files(fixtures.snapshot(root), policy) == protected_files(facts["before"], policy)
    )


def allow_approval(policy: str, command: str, root: Path, facts: dict) -> bool:
    if policy in PROJECT_POLICIES:
        return allow_project_approval(policy, command, root, facts)
    if policy == "rename":
        command = command.strip()
        if command.startswith("cd "):
            prefix, separator, rest = command.partition("&&")
            try:
                parts = shell_parts(prefix)
            except ValueError:
                return False
            directory = parts[1:]
            if directory[:1] == ["--"]:
                directory = directory[1:]
            if not separator or len(directory) != 1 or (root / directory[0]).resolve() != root:
                return False
            command = rest.strip()
        command = re.sub(r"\s*&&\s*ls(?:\s+(?:-la|-al|-l|-a|--))*\s*$", "", command)
        # Only this bounded loop is allowed; substituted shell expressions or
        # extra commands must not ride along with a fixture approval.
        loop = re.fullmatch(
            r'for ([a-zA-Z_]\w*) in (?:\./)?\*\.txt;\s*do\s+mv(?: -v)?(?: --)? '
            r'"\$\1" "\$\{\1%(?:\.txt|\.\*)\}\.md";?\s*done;?',
            command.strip(),
        )
        if loop:
            return True
        try:
            parts = shell_parts(command)
        except ValueError:
            return False
        groups, group = [], []
        for part in parts:
            if part in (";", "&&"):
                if not group:
                    return False
                groups.append(group)
                group = []
            else:
                group.append(part)
        if group:
            groups.append(group)
        for group in groups:
            if not group or group[0] != "mv":
                return False
            args = group[1:]
            while args and args[0] in ("-v", "--"):
                args = args[1:]
            if len(args) != 2 or facts["renames"].get(args[0]) != args[1]:
                return False
        return bool(groups)
    if policy == "cwd":
        try:
            parts = shell_parts(command)
        except ValueError:
            return False
        if not parts or parts.pop(0) != "cd":
            return False
        if parts and parts[0] == "--":
            parts.pop(0)
        if not parts:
            return False
        target = parts.pop(0)
        if (root / target).resolve() != root / "data":
            return False
        if not parts:
            return True
        if len(parts) < 2 or parts[0] not in ("&&", ";") or parts[1] != "ls":
            return False
        return all(arg in (".", "-a", "-l", "-la", "-al", "-lh", "-lah", "--") for arg in parts[2:])
    return False
