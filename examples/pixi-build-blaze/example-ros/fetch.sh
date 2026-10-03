#!/usr/bin/env bash
# Check out ros2/demos (humble, pinned) into src/demos: 21 ROS packages.
set -euo pipefail
cd "$(dirname "$0")"
rev=1fc7a62ee591e4c4a7f84e72aa4b33de11f6fbf3
if [[ ! -d src/demos/.git ]]; then
  mkdir -p src/demos
  git -C src/demos init -q
  git -C src/demos remote add origin https://github.com/ros2/demos
fi
if [[ "$(git -C src/demos rev-parse -q --verify HEAD || true)" != "$rev" ]]; then
  git -C src/demos fetch -q --depth 1 origin "$rev"
  git -C src/demos checkout -q FETCH_HEAD
fi
