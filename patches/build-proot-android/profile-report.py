#!/usr/bin/env python3
"""Summarize the stack samples proot writes with PROOT_PROFILE and PROOT_PROFILE_HZ.

Usage: profile-report.py SAMPLES UNSTRIPPED_PROOT [LIBS_DIR] [TOP]

SAMPLES holds proot's memory map ("m" lines) and one "s" line of return addresses per sample.
Frames in proot are symbolized with UNSTRIPPED_PROOT; frames in other libraries with the file of
the same name in LIBS_DIR, if given (e.g. libc.so pulled from the phone's
/apex/com.android.runtime/lib64/bionic/), else shown as library+offset.

Prints where proot spent its CPU time (self: the sampled frame; inclusive: anywhere on the
stack) and the most frequent call chains.
"""
import bisect
import collections
import os
import shutil
import subprocess
import sys


def symbolize(binary, addresses):
    """Map each address (a virtual address in binary) to its function name."""
    tool = next((t for t in ("llvm-symbolizer", "llvm-symbolizer-19", "llvm-symbolizer-18")
                 if shutil.which(t)), None)
    addresses = sorted(addresses)
    if not tool or not addresses:
        return {}
    query = "\n".join(hex(a) for a in addresses) + "\n"
    out = subprocess.run([tool, "--obj=" + binary, "--functions=linkage", "--inlining=false",
                          "--output-style=GNU"], input=query, capture_output=True,
                         text=True).stdout.splitlines()
    # GNU style: a function name line, then a file:line line, per address.
    return {a: out[i] for a, i in zip(addresses, range(0, len(out), 2)) if out[i] != "??"}


def main():
    samples_path, proot = sys.argv[1], sys.argv[2]
    libs = sys.argv[3] if len(sys.argv) > 3 and os.path.isdir(sys.argv[3]) else None
    top = int(sys.argv[-1]) if len(sys.argv) > 3 and sys.argv[-1].isdigit() else 25

    maps = []  # (start, end, path)
    bases = {}  # path -> load base (lowest mapping)
    samples = []
    for line in open(samples_path):
        if line.startswith("m "):
            fields = line[2:].split()
            start, end = (int(x, 16) for x in fields[0].split("-"))
            path = fields[5] if len(fields) > 5 else ""
            maps.append((start, end, path))
            if path and (path not in bases or start < bases[path]):
                bases[path] = start
        elif line.startswith("s"):
            samples.append([int(x, 16) for x in line.split()[1:]])
    maps.sort()
    starts = [m[0] for m in maps]

    def locate(address):
        i = bisect.bisect_right(starts, address) - 1
        if i >= 0 and maps[i][0] <= address < maps[i][1] and maps[i][2]:
            path = maps[i][2]
            return path, address - bases[path]
        return None, address

    # Return addresses point after the call: step back one instruction for the call site,
    # except for the sampled pc itself (frame 0).
    frames = [[locate(a if i == 0 else a - 4) for i, a in enumerate(s)] for s in samples]
    wanted = collections.defaultdict(set)
    for stack in frames:
        for path, offset in stack:
            if path:
                wanted[path].add(offset)
    names = {}
    for path, offsets in wanted.items():
        base = os.path.basename(path)
        binary = proot if "proot" in base else (os.path.join(libs, base) if libs else None)
        symbols = symbolize(binary, offsets) if binary and os.path.exists(binary) else {}
        for offset in offsets:
            names[(path, offset)] = symbols.get(offset, f"{base}+{offset:#x}")

    stacks = [[names.get(f, f"?{f[1]:#x}") if f[0] else f"?{f[1]:#x}" for f in s] for s in frames]
    total = len(stacks)
    self_counts = collections.Counter(s[0] for s in stacks if s)
    inclusive = collections.Counter()
    for stack in stacks:
        inclusive.update(set(stack))
    chains = collections.Counter(" < ".join(s[:6]) for s in stacks)

    print(f"{total} samples")
    print("\n== self")
    for fn, n in self_counts.most_common(top):
        print(f"{100 * n / total:6.1f}%  {fn}")
    print("\n== inclusive")
    for fn, n in inclusive.most_common(top):
        print(f"{100 * n / total:6.1f}%  {fn}")
    print("\n== call chains (leaf first)")
    for chain, n in chains.most_common(top):
        print(f"{100 * n / total:6.1f}%  {chain}")


if __name__ == "__main__":
    main()
