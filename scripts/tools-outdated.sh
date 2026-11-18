#!/usr/bin/env bash
# Usage: GH_TOKEN=<token> scripts/tools-outdated.sh
set -Eeuo pipefail

cd "${BASH_SOURCE[0]%/*}/.."

stale=0
report() {
  printf 'outdated: %s\n' "$*"
  stale=$((stale + 1))
}

check() {
  if [[ "$2" != "$3" ]]; then
    report "$1 $2 -> $3"
  else
    printf 'current: %s %s\n' "$1" "$2"
  fi
}

pin() { awk -v n="$1" '$1 == n { print $2 }' .tool-versions; }

github() {
  local auth=()
  [[ -n "${GH_TOKEN:-}" ]] && auth=(-H "Authorization: Bearer $GH_TOKEN")
  curl -fsSL --proto '=https' ${auth[@]+"${auth[@]}"} -H 'Accept: application/vnd.github+json' "https://api.github.com/$1"
}

read -r -d '' PICK <<'PYTHON' || true
import datetime, json, re, sys
cutoff = datetime.datetime.now(datetime.timezone.utc) - datetime.timedelta(hours=72)
stable = re.compile(r"v?([0-9]+)\.([0-9]+)\.([0-9]+)")
data = json.load(sys.stdin)
if sys.argv[1] == "github":
    found = [(r["tag_name"], r["published_at"]) for r in data
             if not r["draft"] and not r["prerelease"] and r["published_at"]]
else:
    found = [(v, data["time"][v]) for v in data["versions"] if v in data["time"]]
ready = [(tuple(map(int, m.groups())), tag) for tag, when in found
         if (m := stable.fullmatch(tag))
         and datetime.datetime.fromisoformat(when) <= cutoff]
print(max(ready)[1].removeprefix("v"))
PYTHON

github_latest() { github "repos/$1/releases?per_page=100" | python3 -I -c "$PICK" github; }
npm_latest() { curl -fsSL --proto '=https' "https://registry.npmjs.org/$1" | python3 -I -c "$PICK" npm; }

latest=$(github_latest rust-lang/rust)
check rust "$(pin rust)" "$latest"
latest=$(github_latest EmbarkStudios/cargo-deny)
check cargo-deny "$(pin cargo-deny)" "$latest"
latest=$(github_latest gitleaks/gitleaks)
check gitleaks "$(pin gitleaks)" "$latest"
latest=$(npm_latest prettier)
check prettier "$(pin prettier)" "$latest"
latest=$(npm_latest markdownlint-cli2)
check markdownlint-cli2 "$(pin markdownlint-cli2)" "$latest"

channel=$(sed -n 's/^channel = "\(.*\)"$/\1/p' rust-toolchain.toml)
[[ "$channel" == "$(pin rust)" ]] || report "rust-toolchain.toml says $channel, .tool-versions says $(pin rust)"
for tool in prettier markdownlint-cli2; do
  declared=$(python3 -I -c 'import json, sys; print(json.load(open("package.json"))["devDependencies"][sys.argv[1]])' "$tool")
  locked=$(python3 -I -c 'import json, sys; print(json.load(open("package-lock.json"))["packages"]["node_modules/" + sys.argv[1]]["version"])' "$tool")
  [[ "$declared" == "$(pin "$tool")" ]] || report "package.json says $tool $declared, .tool-versions says $(pin "$tool")"
  [[ "$locked" == "$(pin "$tool")" ]] || report "package-lock.json says $tool $locked, .tool-versions says $(pin "$tool")"
done

pins=$(python3 -I scripts/check-workflow-pins.py --list)
while read -r action version sha; do
  latest=$(github_latest "$action")
  check "$action" "$version" "$latest"
  tagged=$(github "repos/$action/commits/v$version" | python3 -I -c 'import json, sys; print(json.load(sys.stdin)["sha"])')
  [[ "$tagged" == "$sha" ]] || report "$action pin $sha is not v$version ($tagged)"
done <<<"$pins"

if ((stale > 0)); then
  printf 'tools-outdated: %s pin(s) to update\n' "$stale" >&2
  exit 1
fi
printf 'tools-outdated: all pins current\n'
