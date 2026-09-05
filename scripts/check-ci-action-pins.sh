#!/usr/bin/env bash
# Reject mutable GitHub Actions references in repository workflows.
set -euo pipefail

repo_root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
found=0
failed=0

for workflow in "$repo_root"/.github/workflows/*.yml "$repo_root"/.github/workflows/*.yaml; do
  [[ -f $workflow ]] || continue
  while IFS= read -r line; do
    if [[ $line =~ ^[[:space:]]*(-[[:space:]]*)?uses:[[:space:]]*([^[:space:]#]+) ]]; then
      found=1
      reference=${BASH_REMATCH[2]}
      sha=${reference##*@}
      if [[ $reference != *@* || ! $sha =~ ^[0-9a-f]{40}$ ]]; then
        printf 'Mutable or invalid action reference in %s: %s\n' "$workflow" "$reference" >&2
        failed=1
      fi
    fi
  done < "$workflow"
done

(( found )) || {
  printf '%s\n' 'No GitHub Actions references found to check.' >&2
  exit 1
}
(( ! failed )) || exit 1
printf '%s\n' 'All GitHub Actions workflow references are pinned to full commit SHAs.'
