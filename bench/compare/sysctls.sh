#!/usr/bin/env bash
# bench/compare/sysctls.sh — apply the kernel sysctls the comparison
# methodology requires. Idempotent. Must be run as root on the
# bench box.
#
# Reverting: most distros restore defaults at boot. To revert in
# place, copy the prior /etc/sysctl.conf into /etc/sysctl.d/99-... and
# `sysctl --system`.
set -euo pipefail

if [[ "$(id -u)" -ne 0 ]]; then
    echo "must be run as root (writes /proc/sys/...)" >&2
    exit 1
fi

apply() {
    local key=$1 val=$2
    if ! sysctl -w "$key=$val" >/dev/null; then
        echo "FAILED to set $key=$val" >&2
        return 1
    fi
    printf '%-44s = %s\n' "$key" "$val"
}

apply net.core.somaxconn 65535
apply net.core.netdev_max_backlog 65535
apply net.ipv4.tcp_max_syn_backlog 65535
apply net.ipv4.tcp_tw_reuse 1
apply net.ipv4.tcp_fin_timeout 15
apply net.ipv4.tcp_keepalive_time 60
apply net.ipv4.ip_local_port_range "1024 65535"
apply fs.file-max 1048576

echo
echo "ulimit -n recommendation: 1048576 (set per-shell with 'ulimit -n 1048576')"
echo "verify Turbo Boost: cat /sys/devices/system/cpu/intel_pstate/no_turbo  (1 = disabled)"
