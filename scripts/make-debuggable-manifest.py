#!/usr/bin/env python3
"""Writes a copy of the manifest with android:debuggable="true" added.

Kept as its own file so the shell build script does not have to nest a
heredoc inside a heredoc. Only needed for the diagnostic build variant, since
`adb shell run-as` refuses to touch a non-debuggable package's data.

Usage: make-debuggable-manifest.py IN.xml OUT.xml
"""

import sys

ATTRIBUTE = 'android:debuggable="true"'
# Anchored on a stable, already-present attribute rather than the <application>
# tag itself, so the insertion point does not shift if attributes are reordered.
ANCHOR = 'android:allowBackup="false"'


def main() -> int:
    if len(sys.argv) != 3:
        print(__doc__, file=sys.stderr)
        return 2
    src, dst = sys.argv[1], sys.argv[2]
    with open(src, encoding="utf-8") as f:
        text = f.read()
    if ANCHOR not in text:
        print(f"error: {ANCHOR} not found in {src}; update this script", file=sys.stderr)
        return 1
    text = text.replace(ANCHOR, f"{ANCHOR}\n        {ATTRIBUTE}", 1)
    with open(dst, "w", encoding="utf-8") as f:
        f.write(text)
    return 0


if __name__ == "__main__":
    sys.exit(main())
