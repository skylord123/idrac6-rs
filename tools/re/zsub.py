"""Put decrypted strings back into CFR-decompiled source.

usage: python3 zsub.py <zstrings.tsv> <decompiled-dir> <output-dir>

Replaces `z[3]`-style references to a class's encrypted string array with
the plain literal from the TSV written by ZDump.java.
"""
import collections
import os
import re
import sys

tsv, src, dst = sys.argv[1:4]
strings = collections.defaultdict(lambda: collections.defaultdict(dict))
for line in open(tsv, encoding="utf-8"):
    cls, field, idx, lit = line.rstrip("\n").split("\t", 3)
    strings[cls][field][int(idx)] = lit

for root, _, files in os.walk(src):
    for f in files:
        if not f.endswith(".java"):
            continue
        path = os.path.join(root, f)
        rel = os.path.relpath(path, src)
        cls = rel[:-5].replace(os.sep, ".")
        text = open(path, encoding="utf-8").read()
        for field, values in strings.get(cls, {}).items():
            if -1 in values and len(values) == 1:
                continue  # plain String field, references stay readable
            pattern = r"(?<![\w.])" + re.escape(field) + r"\[(\d+)\]"
            text = re.sub(pattern, lambda m: values.get(int(m.group(1)), m.group(0)), text)
        out = os.path.join(dst, rel)
        os.makedirs(os.path.dirname(out), exist_ok=True)
        open(out, "w", encoding="utf-8").write(text)
