#!/usr/bin/env bash
# Deploy, upgrade or remove hdrcast-signal (the HdrCast P2P signaling server) on a remote Linux box.
#
#   signal/deploy.sh --ssh ubuntu@101.43.23.94 [--identity FILE] [--port 18650] [--echo-port 18651] [--dry-run]
#   signal/deploy.sh --ssh ubuntu@101.43.23.94 [--identity FILE] --uninstall [--dry-run]
#
# Changes on the target only /opt/hdrcast-signal/, /etc/systemd/system/hdrcast-signal.service and the
# service's own state directory (created by systemd). Needs python3 >= 3.6, systemd >= 235 and
# passwordless sudo for the ssh user; ssh runs non-interactively (BatchMode).
set -euo pipefail

UNIT_NAME=hdrcast-signal.service
INSTALL_DIR=/opt/hdrcast-signal
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SERVER_PY="$HERE/hdrcast_signal.py"
UNIT_FILE="$HERE/hdrcast-signal.service"

SSH_TARGET=""
IDENTITY=""
PORT=18650
ECHO_PORT=18651
DRY_RUN=0
UNINSTALL=0

usage() {
  cat <<EOF
Usage: $0 --ssh USER@HOST [--identity FILE] [--port N] [--echo-port N] [--dry-run]
       $0 --ssh USER@HOST [--identity FILE] --uninstall [--dry-run]

  --ssh USER@HOST   target; ssh runs in BatchMode and the user needs passwordless sudo
  --identity FILE   ssh private key (for example ~/.ssh/MyServer.pem)
  --port N          HTTP API port, TCP (default 18650)
  --echo-port N     UDP echo port (default 18651, 0 disables the echo)
  --dry-run         print the commands and remote scripts instead of running them
  --uninstall       stop and remove the service (the group registry is kept)
EOF
}

die() { echo "deploy.sh: $*" >&2; exit 1; }
step() { printf '\n==> %s\n' "$*"; }
show() { printf '+'; printf ' %q' "$@"; printf '\n'; }

# ------------------------------------------------------------------ remote scripts (run with bash -s)
# Each script is wrapped in main() so bash has read all of it before anything runs.

read -r -d '' REMOTE_PREFLIGHT <<'REMOTE' || true
set -euo pipefail
fail() { echo "ERROR: $*" >&2; exit 1; }

check_port() {  # proto port our_main_pid
  local proto="$1" port="$2" our_pid="$3" flags lines pids
  if [ "$proto" = tcp ]; then flags=-ltnp; else flags=-lunp; fi
  lines="$(sudo -n ss "$flags" "sport = :$port" </dev/null | tail -n +2)" || fail "ss failed"
  [ -n "$lines" ] || return 0
  pids="$(printf '%s\n' "$lines" | grep -o 'pid=[0-9]*' | cut -d= -f2 | sort -u | tr '\n' ' ' || true)"
  pids="${pids% }"
  if [ -n "$our_pid" ] && [ "$our_pid" != 0 ] && [ "$pids" = "$our_pid" ]; then
    echo "$proto/$port is held by hdrcast-signal itself (upgrade)"
    return 0
  fi
  printf '%s\n' "$lines" >&2
  fail "$proto port $port is already in use by another program; pick another one with --port / --echo-port"
}

main() {
  local port="$1" echo_port="$2" unit=hdrcast-signal.service systemd_version our_pid=""
  [ -x /usr/bin/python3 ] || fail "/usr/bin/python3 not found (the unit runs it); install python3 first"
  /usr/bin/python3 -c 'import sys; sys.exit(0 if sys.version_info >= (3, 6) else 1)' </dev/null \
    || fail "python3 >= 3.6 required, found: $(/usr/bin/python3 -V 2>&1)"
  command -v systemctl >/dev/null 2>&1 || fail "systemd is required"
  systemd_version="$(systemctl --version | awk 'NR == 1 {print $2}')"
  case "$systemd_version" in ''|*[!0-9]*) fail "cannot read the systemd version";; esac
  [ "$systemd_version" -ge 235 ] || fail "systemd >= 235 required (StateDirectory=, DynamicUser=), found $systemd_version"
  command -v ss >/dev/null 2>&1 || fail "ss (iproute2) not found"
  sudo -n true </dev/null 2>/dev/null || fail "passwordless sudo is required for $(id -un)"
  if systemctl is-active --quiet "$unit"; then
    our_pid="$(systemctl show -p MainPID --value "$unit" 2>/dev/null || true)"
  fi
  check_port tcp "$port" "$our_pid"
  [ "$echo_port" = 0 ] || check_port udp "$echo_port" "$our_pid"
  echo "python3 $(/usr/bin/python3 -c 'import platform; print(platform.python_version())'), systemd $systemd_version"
  if [ -e "/etc/systemd/system/$unit" ]; then echo "MODE=upgrade"; else echo "MODE=install"; fi
  echo "TMP=$(mktemp -d /tmp/hdrcast-signal.XXXXXX)"
}
main "$@"
REMOTE

read -r -d '' REMOTE_INSTALL <<'REMOTE' || true
set -euo pipefail
fail() { echo "ERROR: $*" >&2; exit 1; }

health() {  # port
  if command -v curl >/dev/null 2>&1; then
    curl -fsS --max-time 2 "http://127.0.0.1:$1/v1/health"
  else
    /usr/bin/python3 - "$1" <<'PY'
import sys, urllib.request
response = urllib.request.urlopen("http://127.0.0.1:%s/v1/health" % sys.argv[1], timeout=2)
sys.stdout.write(response.read().decode("utf-8"))
PY
  fi
}

main() {
  local tmp="$1" port="$2" unit=hdrcast-signal.service dir=/opt/hdrcast-signal out=""
  case "$tmp" in /tmp/hdrcast-signal.*) ;; *) fail "unexpected temp dir: $tmp";; esac
  # shellcheck disable=SC2064  # expand now: $tmp is local to main() and gone when the trap runs
  trap "rm -rf $(printf '%q' "$tmp")" EXIT
  [ -f "$tmp/hdrcast_signal.py" ] && [ -f "$tmp/$unit" ] || fail "uploaded files missing in $tmp"
  # refuse before touching anything if this python3 cannot even load the server
  PYTHONDONTWRITEBYTECODE=1 /usr/bin/python3 "$tmp/hdrcast_signal.py" --help </dev/null >/dev/null \
    || fail "the server does not start with $(/usr/bin/python3 -V 2>&1); nothing was changed"
  sudo -n install -d -m 0755 -o root -g root "$dir" </dev/null
  sudo -n install -m 0644 -o root -g root "$tmp/hdrcast_signal.py" "$dir/hdrcast_signal.py" </dev/null
  sudo -n install -m 0644 -o root -g root "$tmp/$unit" "/etc/systemd/system/$unit" </dev/null
  sudo -n systemctl daemon-reload </dev/null
  sudo -n systemctl reset-failed "$unit" </dev/null 2>/dev/null || true
  if systemctl is-active --quiet "$unit"; then
    sudo -n systemctl enable --quiet "$unit" </dev/null
    sudo -n systemctl restart "$unit" </dev/null
    echo "restarted $unit"
  else
    sudo -n systemctl enable --quiet --now "$unit" </dev/null
    echo "enabled and started $unit"
  fi
  for _ in $(seq 1 30); do
    if out="$(health "$port" 2>/dev/null)"; then break; fi
    out=""
    sleep 0.5
  done
  if [ -z "$out" ]; then
    systemctl status --no-pager "$unit" </dev/null || true
    sudo -n journalctl -u "$unit" -n 30 --no-pager </dev/null || true
    fail "health check on 127.0.0.1:$port failed"
  fi
  echo "health on the server: $out"
}
main "$@"
REMOTE

read -r -d '' REMOTE_UNINSTALL <<'REMOTE' || true
set -euo pipefail
fail() { echo "ERROR: $*" >&2; exit 1; }

main() {
  local unit=hdrcast-signal.service
  sudo -n true </dev/null 2>/dev/null || fail "passwordless sudo is required for $(id -un)"
  if [ -e "/etc/systemd/system/$unit" ] || systemctl is-active --quiet "$unit"; then
    sudo -n systemctl disable --quiet --now "$unit" </dev/null || true
  fi
  sudo -n rm -f "/etc/systemd/system/$unit" </dev/null
  sudo -n rm -rf /opt/hdrcast-signal </dev/null
  sudo -n systemctl daemon-reload </dev/null
  sudo -n systemctl reset-failed "$unit" </dev/null 2>/dev/null || true
  echo "removed $unit and /opt/hdrcast-signal"
  if sudo -n test -e /var/lib/private/hdrcast-signal </dev/null; then
    echo "kept the group registry in /var/lib/private/hdrcast-signal"
    echo "(to delete it too: sudo rm -rf /var/lib/private/hdrcast-signal)"
  fi
}
main "$@"
REMOTE

# ------------------------------------------------------------------ local side

remote() {  # script args...: run a script on the target; its arguments become $1.. there
  local script="$1" command="bash -s --"
  shift
  if [ $# -gt 0 ]; then
    command="$command$(printf ' %q' "$@")"
  fi
  if [ "$DRY_RUN" = 1 ]; then
    show ssh "${SSH_OPTS[@]}" "$SSH_TARGET" "$command"
    printf '%s\n' "$script" | sed 's/^/    | /'
    return 0
  fi
  # shellcheck disable=SC2029  # $command is built (and %q-quoted) here on purpose
  printf '%s\n' "$script" | ssh "${SSH_OPTS[@]}" "$SSH_TARGET" "$command"
}

render_unit() {  # output path: the unit with ExecStart= rewritten for the chosen ports
  local out="$1" line count
  local exec_line="ExecStart=/usr/bin/python3 $INSTALL_DIR/hdrcast_signal.py --listen [::]:$PORT --echo-port $ECHO_PORT --state-dir \${STATE_DIRECTORY}"
  count="$(grep -c '^ExecStart=' "$UNIT_FILE" || true)"
  [ "$count" = 1 ] || die "$UNIT_FILE must contain exactly one ExecStart= line"
  while IFS= read -r line || [ -n "$line" ]; do
    case "$line" in
      ExecStart=*) printf '%s\n' "$exec_line" ;;
      *) printf '%s\n' "$line" ;;
    esac
  done <"$UNIT_FILE" >"$out"
}

main() {
  while [ $# -gt 0 ]; do
    case "$1" in
      --ssh) [ $# -ge 2 ] || die "--ssh needs a value"; SSH_TARGET="$2"; shift 2 ;;
      --identity) [ $# -ge 2 ] || die "--identity needs a value"; IDENTITY="$2"; shift 2 ;;
      --port) [ $# -ge 2 ] || die "--port needs a value"; PORT="$2"; shift 2 ;;
      --echo-port) [ $# -ge 2 ] || die "--echo-port needs a value"; ECHO_PORT="$2"; shift 2 ;;
      --dry-run) DRY_RUN=1; shift ;;
      --uninstall) UNINSTALL=1; shift ;;
      -h|--help) usage; exit 0 ;;
      *) usage >&2; die "unknown argument: $1" ;;
    esac
  done
  [ -n "$SSH_TARGET" ] || { usage >&2; die "--ssh is required"; }
  case "$SSH_TARGET" in -*|*[[:space:]]*) die "bad --ssh value: $SSH_TARGET" ;; esac
  [[ "$PORT" =~ ^[1-9][0-9]{0,4}$ ]] && [ "$PORT" -le 65535 ] || die "--port must be 1..65535"
  [[ "$ECHO_PORT" =~ ^(0|[1-9][0-9]{0,4})$ ]] && [ "$ECHO_PORT" -le 65535 ] || die "--echo-port must be 0..65535"
  if [ -n "$IDENTITY" ] && [ "$DRY_RUN" = 0 ] && [ ! -r "$IDENTITY" ]; then
    die "cannot read the identity file $IDENTITY"
  fi
  SSH_OPTS=(-o BatchMode=yes -o ConnectTimeout=15 -o ServerAliveInterval=15)
  if [ -n "$IDENTITY" ]; then
    SSH_OPTS+=(-i "$IDENTITY" -o IdentitiesOnly=yes)
  fi
  local host="${SSH_TARGET#*@}"

  if [ "$UNINSTALL" = 1 ]; then
    step "removing hdrcast-signal from $SSH_TARGET"
    remote "$REMOTE_UNINSTALL"
    return 0
  fi

  [ -f "$SERVER_PY" ] && [ -f "$UNIT_FILE" ] || die "missing $SERVER_PY or $UNIT_FILE"
  if command -v python3 >/dev/null 2>&1; then
    python3 -c 'import ast, sys; ast.parse(open(sys.argv[1], "rb").read())' "$SERVER_PY" \
      || die "$SERVER_PY has a syntax error"
  fi
  LOCAL_TMP="$(mktemp -d "${TMPDIR:-/tmp}/hdrcast-signal.XXXXXX")"
  trap 'rm -rf "$LOCAL_TMP"' EXIT
  cp "$SERVER_PY" "$LOCAL_TMP/hdrcast_signal.py"
  render_unit "$LOCAL_TMP/$UNIT_NAME"

  step "preflight on $SSH_TARGET (python3, systemd, sudo, TCP $PORT$([ "$ECHO_PORT" = 0 ] || echo ", UDP $ECHO_PORT"))"
  local mode="install" remote_tmp="/tmp/hdrcast-signal.XXXXXX" output line
  if [ "$DRY_RUN" = 1 ]; then
    remote "$REMOTE_PREFLIGHT" "$PORT" "$ECHO_PORT"
  else
    output="$(remote "$REMOTE_PREFLIGHT" "$PORT" "$ECHO_PORT")" || die "preflight failed; nothing was changed"
    while IFS= read -r line; do
      case "$line" in
        MODE=*) mode="${line#MODE=}" ;;
        TMP=*) remote_tmp="${line#TMP=}" ;;
        *) echo "$line" ;;
      esac
    done <<<"$output"
    case "$remote_tmp" in /tmp/hdrcast-signal.*) ;; *) die "preflight did not create a temp dir" ;; esac
  fi

  step "copying hdrcast_signal.py and $UNIT_NAME ($mode)"
  if [ "$DRY_RUN" = 1 ]; then
    show scp "${SSH_OPTS[@]}" -q "$LOCAL_TMP/hdrcast_signal.py" "$LOCAL_TMP/$UNIT_NAME" "$SSH_TARGET:$remote_tmp/"
    echo "    rendered: $(grep '^ExecStart=' "$LOCAL_TMP/$UNIT_NAME")"
  elif ! scp "${SSH_OPTS[@]}" -q "$LOCAL_TMP/hdrcast_signal.py" "$LOCAL_TMP/$UNIT_NAME" "$SSH_TARGET:$remote_tmp/"; then
    # shellcheck disable=SC2029
    ssh "${SSH_OPTS[@]}" "$SSH_TARGET" "rm -rf $(printf '%q' "$remote_tmp")" </dev/null || true
    die "scp failed; nothing was changed"
  fi

  step "installing into $INSTALL_DIR and /etc/systemd/system, then starting"
  remote "$REMOTE_INSTALL" "$remote_tmp" "$PORT"

  step "done"
  echo "Tencent Cloud security group (安全组): allow inbound TCP $PORT (HTTP API)$([ "$ECHO_PORT" = 0 ] || echo " and UDP $ECHO_PORT (echo)")"
  echo "for 0.0.0.0/0 (plus ::/0 if the instance has IPv6). The host firewall is not touched by this script."
  echo "External check:  curl http://$host:$PORT/v1/health     (expect {\"ok\":true,\"v\":1})"
  echo "Invite codes use \"s\":\"http://$host:$PORT\"."
}

if [ "${BASH_SOURCE[0]}" = "$0" ]; then
  main "$@"
fi
