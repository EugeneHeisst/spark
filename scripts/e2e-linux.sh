#!/usr/bin/env bash
set -euo pipefail
# Invoke inside dbus-run-session AND xvfb-run. The WM must never share the live
# desktop's display or session bus. Native window enumeration needs a real WM.
if [[ "${SPARK_ISOLATED_DISPLAY:-}" != "1" ]]; then
  echo 'Use npm run test:e2e:linux to create an isolated display and session bus.' >&2
  exit 1
fi
export WAYLAND_DISPLAY=
export XDG_SESSION_TYPE=x11
export QT_QPA_PLATFORM=xcb
mkdir -p artifacts
wm_pid=
if command -v openbox >/dev/null; then
  openbox >artifacts/window-manager.log 2>&1 &
  wm_pid=$!
elif command -v kwin_x11 >/dev/null; then
  kwin_x11 --no-kactivities >artifacts/window-manager.log 2>&1 &
  wm_pid=$!
else
  echo 'Install openbox for real window-capture tests.' >&2
  exit 1
fi
trap 'if [[ -n "$wm_pid" ]]; then kill "$wm_pid" 2>/dev/null || true; fi' EXIT
npm run test:e2e
