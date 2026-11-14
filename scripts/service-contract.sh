#!/usr/bin/env bash
# `install-service` against the real service manager (#544): launchd on
# macOS, a systemd user manager on Linux. The stand-ins the tests use take the
# manager's commands at their word, so #542's race (`launchctl bootout`
# returns before the program has ended, and a `bootstrap` meanwhile fails)
# showed only on a real Mac. CI's `Service managers` jobs run both steps
# (development.md §13.8; decision 64).
#
#   contract  The adapter's contract against the real manager
#             (`the_real_manager_meets_the_contract`), on a service of a name
#             of its own whose program and home are scratch folders: it
#             leaves an installed Ladderback service alone.
#   install   The real binary: `ladderback install-service`; the service
#             runs and answers on 127.0.0.1:9898; `install-service` again over
#             the running one, which must have stopped by the time it returns,
#             and a new process answers; then `remove-service`, after which
#             nothing of it runs.
#
# Both fail, never skip, when no user service manager answers: a skip would
# hide what they exist to catch. Both start services in your session, so they
# run only with LADDERBACK_TEST_REAL_SERVICE_MANAGER=1, as the contract test
# does. `install` installs Ladderback's own service, under its real name,
# whose daemon keeps its configuration and data in your real folders, so it
# also needs LADDERBACK_TEST_INSTALL_SERVICE=1, which says this machine is a
# throwaway one such as a CI runner; and it refuses to start where a
# Ladderback service is installed or anything listens on its port.
#
# Usage: scripts/service-contract.sh contract|install
# Exit status: 0 when the step passed; 1 when it failed or refused; 2 on bad
# usage.
set -euo pipefail

cd "$(dirname "$0")/.." || exit 1

HOST=127.0.0.1
PORT=9898
ADDRESS=$HOST:$PORT
# How long a daemon the manager started has to answer.
START_SECONDS=60

fail() {
  echo "service-contract: $*" >&2
  exit 1
}

usage() {
  echo "usage: scripts/service-contract.sh contract|install" >&2
  exit 2
}

[ $# -eq 1 ] || usage
step=$1
case "$step" in
  contract | install) ;;
  *) usage ;;
esac

[ "${LADDERBACK_TEST_REAL_SERVICE_MANAGER:-}" = 1 ] ||
  fail "this starts services in your session; set LADDERBACK_TEST_REAL_SERVICE_MANAGER=1 to run it"

os=$(uname -s)
uid=$(id -u)
case "$os" in
  Darwin)
    label=io.github.nledford.ladderback
    agent="gui/$uid/$label"
    file="$HOME/Library/LaunchAgents/$label.plist"
    ;;
  Linux)
    unit=ladderback.service
    # A relative XDG_CONFIG_HOME is ignored, as install-service ignores it.
    config=${XDG_CONFIG_HOME:-}
    case "$config" in /*) ;; *) config="$HOME/.config" ;; esac
    file="$config/systemd/user/$unit"
    # The link `systemctl --user enable` makes, which removal takes away.
    wants="$config/systemd/user/default.target.wants/$unit"
    ;;
  *) fail "no service manager this checks on $os" ;;
esac

# Whether the user's service manager answers: a GUI session on macOS, a
# systemd user manager on Linux.
manager_answers() {
  case "$os" in
    Darwin) launchctl print "gui/$uid" >/dev/null 2>&1 ;;
    Linux) systemctl --user show-environment >/dev/null 2>&1 ;;
  esac
}

manager_answers || fail "no user service manager answers here (no GUI session on macOS; on Linux, no systemd user manager on XDG_RUNTIME_DIR's bus)"

# Absolute, because test binaries run in their package's directory.
target=${CARGO_TARGET_DIR:-target}
mkdir -p "$target"
target=$(cd "$target" && pwd)

contract() {
  local skips
  skips=$(mktemp "$target/skipped-tests.XXXXXX")
  # shellcheck disable=SC2064 # the path is fixed now, on purpose
  trap "rm -f '$skips'" EXIT
  LADDERBACK_SKIP_LOG=$skips cargo nextest run --workspace --no-tests=fail \
    -E 'package(platform) & test(=service::tests::the_real_manager_meets_the_contract)'
  if [ -s "$skips" ]; then
    cat "$skips" >&2
    fail "the contract skipped itself, which here counts as a failure"
  fi
}

# The process ID of Ladderback's service, as its manager tells it; empty when
# the manager doesn't have it running.
service_pid() {
  local pid
  case "$os" in
    Darwin)
      pid=$(launchctl print "$agent" 2>/dev/null | awk '$1 == "pid" && $2 == "=" { print $3; exit }') || true
      ;;
    Linux)
      pid=$(systemctl --user show --property MainPID --value "$unit" 2>/dev/null) || true
      [ "$pid" != 0 ] || pid=
      ;;
  esac
  echo "$pid"
}

# Whether the manager has the service at all: loaded, on Linux, in any state.
manager_has() {
  case "$os" in
    Darwin) launchctl print "$agent" >/dev/null 2>&1 ;;
    Linux) [ "$(systemctl --user show --property LoadState --value "$unit" 2>/dev/null)" != not-found ] ;;
  esac
}

# Whether a Ladderback daemon answers on ADDRESS: its session names a build.
answers() {
  curl -fsS --max-time 5 "http://$ADDRESS/api/v1/session" 2>/dev/null | grep -q '"version"'
}

# Whether anything at all listens on ADDRESS, whatever it speaks.
port_taken() {
  (exec 3<>"/dev/tcp/$HOST/$PORT") 2>/dev/null
}

alive() {
  kill -0 "$1" 2>/dev/null
}

# Waits until the manager has the service running in a process other than
# `$1` (empty for any) and the daemon answers, then prints its process ID.
wait_running() {
  local not=$1 pid deadline=$((SECONDS + START_SECONDS))
  while [ "$SECONDS" -lt "$deadline" ]; do
    pid=$(service_pid)
    if [ -n "$pid" ] && [ "$pid" != "$not" ] && answers; then
      echo "$pid"
      return 0
    fi
    sleep 1
  done
  fail "the service didn't run and answer on $ADDRESS within ${START_SECONDS}s (process: ${pid:-none})"
}

# A failed install step says what the manager and the daemon said, and
# removes what it installed.
after_install() {
  local status=$?
  [ "$status" -ne 0 ] || return 0
  echo "--- what the service manager says" >&2
  case "$os" in
    Darwin)
      launchctl print "$agent" >&2 || true
      echo "--- the daemon's log" >&2
      tail -n 50 "$HOME/Library/Logs/ladderback.log" >&2 || true
      ;;
    Linux)
      systemctl --user status "$unit" --no-pager >&2 || true
      journalctl --user --unit "$unit" --no-pager --lines 50 >&2 || true
      ;;
  esac
  [ ! -e "$file" ] || "$bin" remove-service || true
}

install() {
  [ "${LADDERBACK_TEST_INSTALL_SERVICE:-}" = 1 ] ||
    fail "this installs Ladderback's real service, whose daemon uses your real configuration and data; set LADDERBACK_TEST_INSTALL_SERVICE=1 only on a throwaway machine such as a CI runner"
  [ ! -e "$file" ] || fail "a Ladderback service is installed here ($file); this step replaces and removes it, so it runs only where none is"
  ! manager_has || fail "the service manager has a Ladderback service; this step runs only where none is"
  ! port_taken || fail "something listens on $ADDRESS already, which the service needs"

  # `--workspace`, as the Justfile's builds: Cargo unifies features over the
  # packages it builds, so the root package alone would rebuild the shared
  # crates with other features than the contract's build and the cache have.
  cargo build --workspace --bin ladderback
  bin="$target/debug/ladderback"
  trap after_install EXIT

  echo "--- install-service"
  "$bin" install-service || fail "install-service failed"
  [ -f "$file" ] || fail "install-service wrote no service file at $file"
  local first
  first=$(wait_running "")
  echo "the service runs as process $first and answers on $ADDRESS"

  # #542: an install over the running service must stop the old daemon before
  # it starts the new one, and has once it returns.
  echo "--- install-service over the running service"
  "$bin" install-service || fail "install-service over the running service failed"
  ! alive "$first" || fail "install-service returned while the old daemon (process $first) still ran"
  local second
  second=$(wait_running "$first")
  echo "the new service runs as process $second and answers on $ADDRESS"

  echo "--- remove-service"
  "$bin" remove-service || fail "remove-service failed"
  ! alive "$second" || fail "remove-service returned while the daemon (process $second) still ran"
  ! manager_has || fail "the service manager still has the service after remove-service"
  [ ! -e "$file" ] || fail "remove-service left the service file at $file"
  if [ "$os" = Linux ] && { [ -e "$wants" ] || [ -L "$wants" ]; }; then
    fail "remove-service left the service enabled ($wants)"
  fi
  ! port_taken || fail "something still listens on $ADDRESS after remove-service"
  echo "service-contract: install, reinstall over the running service and removal passed"
}

"$step"
