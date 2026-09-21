#!/usr/bin/env python3
"""Dump the studio window's accessibility tree.

This is how the studio's UI is verified. A screenshot proves pixels were drawn; this proves
the window is *readable* — that every clip, track, gap, marker, finding and control is an
element with a role and a name, which is what a screen reader consumes. It is also the only
verification that works while the session is locked or over ssh.

Usage:

    dvs-studio --project /path/to/promo &        # WEBKIT_DISABLE_DMABUF_RENDERER=1 on wlroots
    python3 scripts/a11y-tree.py                 # whole tree
    python3 scripts/a11y-tree.py --grep "table cell"

Requires `at-spi2-core` and `python-gobject`, both of which any GNOME/GTK desktop already
has, and an enabled accessibility bus (`gsettings get org.gnome.desktop.interface
toolkit-accessibility` → true).
"""

import argparse
import sys

import gi

gi.require_version("Atspi", "2.0")
from gi.repository import Atspi  # noqa: E402  (must follow require_version)

# Roles that carry no information of their own in this UI; printing them buries the rows
# that matter under a wall of anonymous wrappers.
NOISE = {
    "filler",
    "section",
    "static",
    "paragraph",
    "scroll pane",
    "scroll bar",
    "description term",
    "description value",
}


def find_app(name: str):
    desktop = Atspi.get_desktop(0)
    for index in range(desktop.get_child_count()):
        app = desktop.get_child_at_index(index)
        if app is not None and name.lower() in (app.get_name() or "").lower():
            return app
    return None


def walk(node, depth: int, budget: list, out: list, width: int) -> None:
    if budget[0] <= 0:
        return
    budget[0] -= 1
    role = node.get_role_name()
    label = (node.get_name() or "").strip().replace("\n", " ")
    if len(label) > width:
        label = label[: width - 1] + "…"
    if role not in NOISE:
        out.append("  " * depth + f"{role}: {label}")
        depth += 1
    for index in range(node.get_child_count()):
        child = node.get_child_at_index(index)
        if child is not None:
            walk(child, depth, budget, out, width)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--app", default="dvs-studio", help="application name to look for")
    parser.add_argument("--grep", help="only print lines containing this")
    parser.add_argument("--limit", type=int, default=600, help="maximum nodes to visit")
    parser.add_argument("--width", type=int, default=110, help="truncate names at")
    args = parser.parse_args()

    Atspi.init()
    app = find_app(args.app)
    if app is None:
        print(
            f"no accessible application matching {args.app!r}.\n"
            "Is the window running, and is the accessibility bus enabled?",
            file=sys.stderr,
        )
        return 1

    lines: list = []
    walk(app, 0, [args.limit], lines, args.width)
    if args.grep:
        lines = [line for line in lines if args.grep.lower() in line.lower()]
    print("\n".join(lines))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
