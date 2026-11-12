# Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.
# pyright: reportAttributeAccessIssue=false, reportCallIssue=false

"""BR-001: Auto-compute module dependency order from import statements."""

import ast
import os
import sys
from collections import defaultdict, deque


def canonical_name(rel_path):
    """Convert relative path to dotted module name (without clean_code. prefix)."""
    rel_path = rel_path.replace(chr(92), "/")
    parts = rel_path[:-3].split("/")
    if parts[-1] == "__init__":
        parts.pop()
    return ".".join(parts)


def find_imports(content, path=None):
    """Extract set of clean_code module names imported from content.

    Returns names WITHOUT 'clean_code.' prefix (matching canonical_name output).

    `path` is the file the content came from; it is named in the error below
    so an operator does not have to grep the tree to find the offending module.

    Parsed with `ast` rather than a regular expression, because the text is a
    Python program and not a string to scan: a commented-out import is not an
    import, and an import whose target is a package (`from clean_code import
    config`) names the module inside it. Both shapes below yield every dotted
    path that STARTS at `clean_code`, so the caller can match the longest one
    that exists as a module.
    """
    try:
        tree = ast.parse(content)
    except SyntaxError:
        # A module that does not parse cannot be bundled anyway; the
        # compilation step reports it far better than this would.
        return set()
    imports = set()
    for node in ast.walk(tree):
        if isinstance(node, ast.Import):
            for alias in node.names:
                if alias.name == "clean_code" or alias.name.startswith("clean_code."):
                    imports.add(alias.name)
        elif isinstance(node, ast.ImportFrom):
            if node.level:
                # `from . import x` names a module of THIS package, and the
                # bundle needs the dependency edge to order it. Dropping it
                # silently would leave the edge out of the topological sort,
                # and the single-file bundle then fails at run time on a
                # module that concatenation happened to place later. Refusing
                # here names the offending module instead.
                raise ValueError(
                    f"{path or '<unknown>'}: relative import cannot be "
                    f"ordered by the bundler: from {'.' * node.level}"
                    f"{node.module or ''} import "
                    f"{', '.join(alias.name for alias in node.names)}"
                )
            base = node.module or ""
            if not (base == "clean_code" or base.startswith("clean_code.")):
                continue
            imports.add(base)
            for alias in node.names:
                imports.add(f"{base}.{alias.name}")
    return imports


def resolve_imports(imports, known):
    """Map each imported dotted path onto the modules that actually exist.

    `known` is the set of canonical module names. A path is resolved to the
    LONGEST prefix that is a module, so `clean_code.config` and
    `from clean_code import config` both become `config`, and a package-shaped
    name (`clean_code.handlers`) is not invented when only `clean_code.handlers.
    cmove` exists.
    """
    found = set()
    for dotted in imports:
        if not dotted.startswith("clean_code"):
            continue
        relative = dotted[len("clean_code") :].lstrip(".")
        if not relative:
            continue
        parts = relative.split(".")
        for end in range(len(parts), 0, -1):
            candidate = ".".join(parts[:end])
            if candidate in known:
                found.add(candidate)
                break
    return found


def compute_order(base_dir):
    """Compute topological order of clean_code/ modules from imports.

    Returns list of (canonical_name, relative_path) in dependency order.
    Exits with error on circular imports.
    """
    # Discover all .py files
    modules = {}  # canonical_name -> rel_path
    for root, dirs, files in os.walk(base_dir):
        for f in files:
            if f.endswith(".py"):
                full = os.path.join(root, f)
                rel = os.path.relpath(full, base_dir)
                name = canonical_name(rel)
                modules[name] = rel

    # Build dependency graph
    deps = {}  # canonical_name -> set of canonical_names it depends on
    for name, rel in modules.items():
        full = os.path.join(base_dir, rel)
        with open(full, "r", encoding="utf-8-sig") as fh:
            content = fh.read()
        deps[name] = resolve_imports(
            find_imports(content, path=rel), set(modules.keys())
        )
        deps[name].discard(name)  # No self-dependency

    # Topological sort (Kahn's algorithm)
    # Edge B -> A means B must come before A (A depends on B)
    graph = defaultdict(list)
    in_deg = defaultdict(int)

    for name in modules:
        in_deg[name] = 0

    for name, dep_set in deps.items():
        for dep in dep_set:
            graph[dep].append(name)
            in_deg[name] += 1

    queue = deque([n for n in modules if in_deg[n] == 0])
    order = []

    while queue:
        n = queue.popleft()
        order.append(n)
        for s in graph[n]:
            in_deg[s] -= 1
            if in_deg[s] == 0:
                queue.append(s)

    # Detect cycles
    if len(order) != len(modules):
        remaining = set(modules) - set(order)
        print(
            f"ERROR: Circular import in clean_code/! {len(remaining)} modules in cycle:",
            file=sys.stderr,
        )
        for n in sorted(remaining):
            unresolved = deps[n] & remaining
            print(
                "  {} depends on: {}".format(
                    n, sorted(unresolved) if unresolved else "(none)"
                ),
                file=sys.stderr,
            )
        sys.exit(1)

    return [(n, modules[n]) for n in order]
