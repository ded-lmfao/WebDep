#!/usr/bin/env bash
# =============================================================================
# WebDep one-line installer
#
#   Both (dashboard + host apps on this VPS)   — recommended for 1 VPS:
#     curl -fsSL https://raw.githubusercontent.com/ded-lmfao/WebDep/main/install.sh | sudo bash
#
#   Core only (dashboard, no apps on this VPS):
#     curl -fsSL https://raw.githubusercontent.com/ded-lmfao/WebDep/main/install.sh | sudo bash -s -- core
#
#   Node only (extra VPS joining an existing dashboard):
#     curl -fsSL https://raw.githubusercontent.com/ded-lmfao/WebDep/main/install.sh | sudo bash -s -- node --join http://<CORE_IP>:4600 --token <TOKEN>
#
# Options:
#   --dir <path>        Install folder (default: /opt/webdep)
#   --branch <name>     Git branch to install (default: main)
#   --repo <owner/name> GitHub repository to install from (default: ded-lmfao/WebDep)
#   --join <url>        Node: Core panel URL
#   --token <token>     Node: enrollment token from the panel (Cluster -> Add Node)
#   --name <name>       Node: display name        --region <region>   Node: region label
#   --public-ip <ip>    Node: public IP override
#   --uninstall         Stop and remove WebDep services (keeps the install folder)
#
# Run with no role on an interactive terminal to get a menu (install / update / status / uninstall).
# Re-running the same command repairs/updates an existing install; data/ and .env are kept.
# =============================================================================
set -euo pipefail

# Everything lives in main() so bash reads the whole script before running it:
# under `curl | bash`, child commands could otherwise swallow the rest of the script from stdin.
main() {
REPO="${WEBDEP_REPO:-ded-lmfao/WebDep}"
INSTALL_DIR="${WEBDEP_DIR:-/opt/webdep}"
BRANCH="${WEBDEP_BRANCH:-main}"
ROLE=""
UNINSTALL=0
NODE_ARGS=()
JOIN_URL=""
JOIN_TOKEN=""

say()  { printf '\033[1;36m[webdep]\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33m[webdep]\033[0m %s\n' "$*" >&2; }
die()  { printf '\033[1;31m[webdep] ERROR:\033[0m %s\n' "$*" >&2; exit 1; }

need_value() { [[ -n "${2:-}" && "${2:0:2}" != "--" ]] || die "Option $1 needs a value."; }

while [[ $# -gt 0 ]]; do
    case "$1" in
        both|all|core|node) ROLE="$1" ;;
        --both|--all|--core|--node) ROLE="${1#--}" ;;
        --dir|--path) need_value "$1" "${2:-}"; INSTALL_DIR="$2"; shift ;;
        --branch) need_value "$1" "${2:-}"; BRANCH="$2"; shift ;;
        --repo) need_value "$1" "${2:-}"; REPO="$2"; shift ;;
        --join) need_value "$1" "${2:-}"; JOIN_URL="$2"; shift ;;
        --token) need_value "$1" "${2:-}"; JOIN_TOKEN="$2"; shift ;;
        --name|--region|--public-ip) need_value "$1" "${2:-}"; NODE_ARGS+=("$1" "$2"); shift ;;
        --uninstall|uninstall) UNINSTALL=1 ;;
        -h|--help) sed -n '2,27p' "$0" 2>/dev/null || echo "See https://github.com/${REPO}#readme"; exit 0 ;;
        *) die "Unknown option '$1'. Use: both | core | node [--join URL --token TOKEN] [--dir PATH]" ;;
    esac
    shift
done
[[ "$ROLE" == "all" ]] && ROLE="both"

[[ "$(id -u)" -eq 0 ]] || die "Please run as root. Put 'sudo' before 'bash', e.g.:  curl -fsSL <url> | sudo bash"
[[ "$(uname -s)" == "Linux" ]] || die "This installer supports Linux servers only (Ubuntu/Debian/Fedora/RHEL)."

do_uninstall() {
    say "Stopping and removing WebDep services..."
    for svc in webdep-node webdep-core webdep; do
        systemctl disable --now "$svc" >/dev/null 2>&1 || true
        rm -f "/etc/systemd/system/${svc}.service"
    done
    rm -f /etc/sudoers.d/webdep-firewall
    systemctl daemon-reload >/dev/null 2>&1 || true
    say "Done. Your files are still in ${INSTALL_DIR} (delete that folder to remove everything)."
}

do_status() {
    local svc state any=0
    for svc in webdep-core webdep-node webdep; do
        [[ -f "/etc/systemd/system/${svc}.service" ]] || continue
        any=1
        state=$(systemctl is-active "$svc" 2>/dev/null || true)
        printf '    %-12s %s\n' "$svc" "${state:-unknown}"
    done
    [[ "$any" == "1" ]] || say "WebDep is not installed on this server."
}

# ask <var> <question> [default]  — reads from the keyboard on fd 3 (works under `curl | bash`).
ask() {
    local __var="$1" __q="$2" __def="${3:-}" __ans=""
    if [[ -n "$__def" ]]; then
        read -r -u 3 -p "  ${__q} [${__def}]: " __ans || true
    else
        read -r -u 3 -p "  ${__q}: " __ans || true
    fi
    printf -v "$__var" '%s' "${__ans:-$__def}"
}

if [[ "$UNINSTALL" == "1" ]]; then
    do_uninstall
    exit 0
fi

# Interactive menu when no role was given and a keyboard is attached.
if [[ -z "$ROLE" ]] && ( : </dev/tty ) 2>/dev/null; then
    exec 3</dev/tty
    echo ""
    echo "  ==============================================="
    echo "            WebDep - Easy Installer"
    echo "  ==============================================="
    echo "    1) Install Both  - dashboard + host websites here (best for 1 VPS)"
    echo "    2) Install Core  - dashboard only"
    echo "    3) Install Node  - extra server joining an existing dashboard"
    echo "    4) Update        - get the latest version"
    echo "    5) Status        - show whether WebDep is running"
    echo "    6) Uninstall     - stop and remove the services"
    echo "    0) Exit"
    echo ""
    ask choice "Choose an option" 1
    case "$choice" in
        1) ROLE="both" ;;
        2) ROLE="core" ;;
        3) ROLE="node" ;;
        4) [[ -f "${INSTALL_DIR}/start.sh" ]] || ask INSTALL_DIR "Where is WebDep installed?" "$INSTALL_DIR"
           [[ -f "${INSTALL_DIR}/start.sh" ]] || die "No WebDep install found in ${INSTALL_DIR}."
           ( cd "$INSTALL_DIR" && bash ./start.sh --update </dev/null )
           exit 0 ;;
        5) do_status; exit 0 ;;
        6) ask confirm "Remove WebDep services? Your files stay. Type yes to confirm" "no"
           [[ "$confirm" == "yes" ]] || { say "Cancelled."; exit 0; }
           do_uninstall; exit 0 ;;
        0) exit 0 ;;
        *) die "Invalid choice '${choice}'." ;;
    esac
    ask INSTALL_DIR "Install folder" "$INSTALL_DIR"
    if [[ "$ROLE" == "node" ]]; then
        echo "  (Get the URL and token from your dashboard: Nodes -> + Add Node)"
        [[ -n "$JOIN_URL" ]]   || ask JOIN_URL "Dashboard URL (e.g. http://1.2.3.4:4600)"
        [[ -n "$JOIN_TOKEN" ]] || ask JOIN_TOKEN "Join token"
        ask node_name "Name for this server (optional)" "$(hostname)"
        [[ -n "$node_name" ]] && NODE_ARGS+=(--name "$node_name")
        ask node_region "Region label (optional, e.g. eu-central)" ""
        [[ -n "$node_region" ]] && NODE_ARGS+=(--region "$node_region")
    fi
    exec 3<&-
fi
ROLE="${ROLE:-both}"

if [[ "$ROLE" == "node" ]]; then
    [[ -n "$JOIN_URL" && -n "$JOIN_TOKEN" ]] || die "Node install needs --join <dashboard URL> and --token <token>. Get both from the dashboard: Cluster -> Add Node."
    NODE_ARGS=(--join "$JOIN_URL" --token "$JOIN_TOKEN" "${NODE_ARGS[@]}")
fi

say "Installing WebDep (${ROLE}) into ${INSTALL_DIR} from ${REPO}@${BRANCH}"

# Base tools: git to fetch, curl to download the release binary.
if ! command -v git >/dev/null 2>&1 || ! command -v curl >/dev/null 2>&1; then
    say "Installing git and curl..."
    if command -v apt-get >/dev/null 2>&1; then
        DEBIAN_FRONTEND=noninteractive apt-get update -y >/dev/null
        DEBIAN_FRONTEND=noninteractive apt-get install -y git curl ca-certificates >/dev/null
    elif command -v dnf >/dev/null 2>&1; then
        dnf install -y git curl ca-certificates >/dev/null
    elif command -v yum >/dev/null 2>&1; then
        yum install -y git curl ca-certificates >/dev/null
    else
        die "Could not install git/curl automatically. Install them, then run this command again."
    fi
fi

if [[ -d "${INSTALL_DIR}/.git" ]]; then
    say "Existing install found - updating code (data/ and .env are kept)..."
    git -C "$INSTALL_DIR" fetch --depth 1 origin "$BRANCH"
    git -C "$INSTALL_DIR" checkout -B "$BRANCH" FETCH_HEAD >/dev/null 2>&1 \
        || die "Could not update ${INSTALL_DIR} (local file changes?). Fix or move that folder, then retry."
elif [[ -e "$INSTALL_DIR" && -n "$(ls -A "$INSTALL_DIR" 2>/dev/null)" ]]; then
    die "${INSTALL_DIR} exists and is not a WebDep install. Choose another folder with --dir <path>."
else
    mkdir -p "$(dirname "$INSTALL_DIR")"
    git clone --depth 1 --branch "$BRANCH" "https://github.com/${REPO}.git" "$INSTALL_DIR"
fi

cd "$INSTALL_DIR"
export WEBDEP_REPO="$REPO"

case "$ROLE" in
    both) bash ./start.sh --service </dev/null ;;
    core) bash ./start.sh --service --core </dev/null ;;
    node) bash ./start.sh --service --node "${NODE_ARGS[@]}" </dev/null ;;
esac

echo ""
say "Installed in ${INSTALL_DIR}."
if [[ "$ROLE" != "node" ]]; then
    PORT=$(grep -E '^PORT=' .env 2>/dev/null | cut -d= -f2 | tr -d '[:space:]')
    say "Open the dashboard:  http://<this-server-ip>:${PORT:-4600}"
    say "First-login password: sudo journalctl -u webdep-core | grep -i password   (or see ${INSTALL_DIR}/.env)"
fi
say "Update later: run the same install command again."
}

main "$@"
