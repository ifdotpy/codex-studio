#!/bin/zsh
# shot.sh <name> [install]  - screenshot the Duo inner display, rotated, small copy
U=885F0A85-0BEF-4C21-AFC0-AF85338F7392
APP="/Users/igor/Projects/codex-agents/.worktrees/codex-agents/c6708b22-a9dd-55e2-ab93-e9fcee1a498c/workspaces/client/apps/native/src-tauri/gen/apple/build/arm64-sim/Codex Studio.app"
if [[ $2 == install ]]; then
  xcrun simctl terminate $U local.codex.agents 2>/dev/null
  xcrun simctl install $U "$APP" || exit 1
  SIMCTL_CHILD_STUDIO_SERVER_URL=${URL:-http://127.0.0.1:8793} xcrun simctl launch $U local.codex.agents >/dev/null || exit 1
  sleep ${WAIT:-15}
fi
INNER=$(xcrun simctl io $U enumerate 2>/dev/null | awk '/UUID:/ {u=$2} /Port Identifier: com.apple.framebuffer.display/ {fb=1} /Default width:/ && fb {w=$3} /Default height:/ && fb { if (w >= 1000 && w <= 4000 && w*$3 > best) {best=w*$3; id=u}; fb=0} END {print id}')
mkdir -p /tmp/duo-ios-shots
xcrun simctl io $U screenshot --display=$INNER /tmp/duo-ios-shots/$1.png >/dev/null 2>&1
sips -Z 1400 /tmp/duo-ios-shots/$1.png --out /tmp/duo-ios-shots/$1-small.png >/dev/null && echo /tmp/duo-ios-shots/$1-small.png
