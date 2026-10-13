# Isolated environment for a Studio Rust worker.
# Usage: CS_NAME=worker-name . workspaces/tooling/worker-env.sh
: "${CS_NAME:?set CS_NAME for the worker}"
CS_HOST_HOME=${HOME:?}
CS_ROOT=/var/tmp/cs-$CS_NAME
CS_TMPFS=/dev/shm/cs-$CS_NAME-tmp
mkdir -p "$CS_ROOT/home" "$CS_ROOT/codex" "$CS_ROOT/claude" "$CS_ROOT/cache" "$CS_TMPFS/tmp" "$CS_TMPFS/state"
unset CODEX_AGENTS_PYTHON PYTHON CODEX_BIN
export HOME=$CS_ROOT/home CODEX_HOME=$CS_ROOT/codex CLAUDE_CONFIG_DIR=$CS_ROOT/claude
export XDG_CACHE_HOME=$CS_ROOT/cache
export STUDIO_TEST_ISOLATION_ROOT=$CS_ROOT
export TMPDIR=$CS_TMPFS/tmp
export CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-$CS_ROOT/target}
export RUSTUP_HOME=${RUSTUP_HOME:-$CS_HOST_HOME/.rustup} CARGO_HOME=${CARGO_HOME:-$CS_HOST_HOME/.cargo}
export PATH=/usr/local/bin:/usr/bin:/bin:$CARGO_HOME/bin
export CODEX_NODE=/usr/bin/node
export CARGO_BUILD_JOBS=4 CARGO_INCREMENTAL=0 CARGO_PROFILE_DEV_DEBUG=0 CARGO_PROFILE_TEST_DEBUG=0
