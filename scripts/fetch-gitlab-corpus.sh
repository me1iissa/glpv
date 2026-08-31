#!/usr/bin/env bash
# Fetch the gitlab-org/gitlab pipeline corpus as blobless shallow clones.
# glpv reads files via `git show`, so no checkout is needed; blobs are
# lazily fetched per file read (a few MB total instead of gigabytes).
set -euo pipefail
dir="${1:-corpus}"
mkdir -p "$dir" && cd "$dir"
clone() { # <project-path> [dir]
  local p="$1" d="${2:-$(echo "$1" | tr '/' '-')}"
  [ -d "$d" ] || git clone --filter=blob:none --depth 1 --no-checkout "https://gitlab.com/$p.git" "$d"
}
tags() { git -C "$1" fetch -q --depth 1 --filter=blob:none origin '+refs/tags/*:refs/tags/*'; }

clone gitlab-org/gitlab gitlab
clone gitlab-org/gitlab-foss
clone gitlab-org-sandbox/gitlab-jh-validation
clone gitlab-org/components/danger-review && tags gitlab-org-components-danger-review
clone gitlab-org/analytics-section/analytics-instrumentation/ci-components \
  && tags gitlab-org-analytics-section-analytics-instrumentation-ci-components
clone gitlab-com/gl-infra/api-docs
clone gitlab-com/gl-infra/common-ci-tasks && tags gitlab-com-gl-infra-common-ci-tasks
clone gitlab-com/gl-infra/autolabels || true
# Branch used by the JH code-sync trigger:
git -C gitlab-org-sandbox-gitlab-jh-validation fetch -q --depth 1 --filter=blob:none \
  origin '+refs/heads/as-if-jh-code-sync:refs/remotes/origin/as-if-jh-code-sync' || true

echo "Corpus ready. Scan with:"
echo "  glpv scan --projects $dir --entry gitlab-org/gitlab -o out/"
