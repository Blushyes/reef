#!/usr/bin/env python3
"""Generate the Homebrew formula from the archives uploaded to a Reef release."""

import argparse
import hashlib
import re
from pathlib import Path


def formula(tag: str, archives: Path) -> str:
    blocks: list[str] = []
    for system, platform in (("macos", "darwin"), ("linux", "linux")):
        architectures: list[str] = []
        for architecture, suffix in (("arm", "arm64"), ("intel", "x64")):
            filename = f"reef-{platform}-{suffix}.tar.gz"
            digest = hashlib.sha256((archives / filename).read_bytes()).hexdigest()
            architectures.append(f'''    on_{architecture} do
      url "https://github.com/Blushyes/reef/releases/download/{tag}/{filename}"
      sha256 "{digest}"
    end''')
        blocks.append(f"  on_{system} do\n" + "\n".join(architectures) + "\n  end")

    platforms = "\n\n".join(blocks)
    return f'''class Reef < Formula
  desc "Terminal workspace for browsing files, reviewing diffs, and committing changes"
  homepage "https://github.com/Blushyes/reef"
  version "{tag.removeprefix('v')}"
  license any_of: ["MIT", "Apache-2.0"]

{platforms}

  def install
    bin.install "reef"
    bin.install_symlink "reef" => "rf"
  end

  test do
    assert_match "USAGE:", shell_output("#{{bin}}/reef --help 2>&1")
    assert_match "USAGE:", shell_output("#{{bin}}/rf --help 2>&1")
  end
end
'''


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("tag", help="Stable release tag, e.g. v0.44.1")
    parser.add_argument("archives", type=Path, help="Directory containing all four tarballs")
    parser.add_argument("output", type=Path, help="Destination Formula/reef.rb")
    args = parser.parse_args()
    if re.fullmatch(r"v\d+\.\d+\.\d+", args.tag) is None:
        parser.error("tag must be a stable release version such as v0.44.1")
    args.output.write_text(formula(args.tag, args.archives), encoding="utf-8")


if __name__ == "__main__":
    main()
