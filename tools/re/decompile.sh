#!/usr/bin/env bash
# Download the iDRAC6 viewer from your own iDRAC, decompile it and decrypt
# its obfuscated strings. Output goes to ./re (ignored by git).
#
#   tools/re/decompile.sh <idrac-host>
#
# Needs: curl, java (11+), python3.
set -euo pipefail
host=${1:?usage: $0 <idrac-host>}
here=$(cd "$(dirname "$0")" && pwd)
out=re
mkdir -p "$out/jars" "$out/tools"
curl_legacy() { curl -sSk --tlsv1.0 --tls-max 1.0 --ciphers 'AES128-SHA:@SECLEVEL=0' "$@"; }
for j in avctKVM avctKVMIOLinux64 avctVMLinux64; do
  curl_legacy -o "$out/jars/$j.jar" "https://$host/software/$j.jar"
done
[ -f "$out/tools/cfr.jar" ] || curl -sSL -o "$out/tools/cfr.jar" https://repo1.maven.org/maven2/org/benf/cfr/0.152/cfr-0.152.jar
java -jar "$out/tools/cfr.jar" "$out/jars/avctKVM.jar" --outputdir "$out/src" --silent true
# Strings are XOR-encrypted per class (Zelix KlassMaster style). Load each
# class and read its static string fields, then put the plain strings back
# into the decompiled source.
mkdir -p "$out/tools/zdec"
javac -d "$out/tools/zdec" "$here/ZDump.java"
java -Djava.awt.headless=true -cp "$out/tools/zdec" ZDump "$out/jars/avctKVM.jar" "$out/zstrings.tsv" 2>/dev/null || true
python3 -I "$here/zsub.py" "$out/zstrings.tsv" "$out/src" "$out/srcd"
echo "Readable source: $out/srcd"
