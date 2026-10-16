if [ -z "${TZ:-}" ] && zone=$(readlink /etc/localtime 2>/dev/null); then
    case "$zone" in */zoneinfo/*) export TZ="${zone#*/zoneinfo/}" ;; esac
fi
