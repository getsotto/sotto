#!/bin/sh
# Entrypoint for the edge (Caddy) container: raise TCP's initial congestion window, give up the
# capability it took to do that, then hand over to Caddy.
#
# A new connection may send only its initial window before the first acknowledgements return,
# and Linux starts every connection at 10 segments, about 14 KB. A response larger than that
# waits one more round trip per doubling, however fast the server answered: a machine fetching
# 100 secrets (about 26 KB compressed) paid one extra round trip, and 1,000 secrets paid four.
# At 40 segments, about 58 KB, the first costs none and the second two. Past 40 nothing more is
# gained, because a Linux client's initial receive window is about 64 KB and becomes the limit.
#
# The window is an attribute of a route, looked up in the network namespace of the socket that
# sends, so it has to be set here, inside the container that owns the listening sockets; setting
# it on the host changes nothing for a published port. It is set on every route of both address
# families rather than only the default one, so it holds whichever route the replies leave by.
INITCWND=40

# Whether NET_ADMIN (capability 12) is in this process's bounding set.
holds_net_admin() {
    bounding=$(sed -n 's/^CapBnd:[[:space:]]*//p' /proc/self/status)
    [ $(((0x${bounding:-0} >> 12) & 1)) -eq 1 ]
}

# Second pass, reached through setpriv from the end of the first. setpriv reports success even
# when the kernel refused the drop (it needs SETPCAP), so the bounding set is read back here
# and Caddy starts only if the capability is really gone. An edge that is down is found at once;
# one quietly holding NET_ADMIN is not.
if [ "${1:-}" = --net-admin-dropped ]; then
    shift
    if holds_net_admin; then
        echo "edge: NET_ADMIN could not be removed from the bounding set (that needs SETPCAP); not starting Caddy with it" >&2
        exit 1
    fi
    exec "$@"
fi

# Set the window on each route of one address family, and print how many routes took it. A route
# that refuses is skipped: one odd route should not cost the others their window.
tune() {
    ip "$1" route show 2>/dev/null | sed 's/ initcwnd [0-9]*//' | {
        changed=0
        # Each line is a complete route as `ip route change` wants it back, less any earlier window.
        while read -r route; do
            # shellcheck disable=SC2086 # the route is a list of arguments, and must split into them
            if ip "$1" route change $route initcwnd "$INITCWND" 2>/dev/null; then
                changed=$((changed + 1))
            fi
        done
        echo "$changed"
    }
}

# Tuning never keeps the edge down. Without NET_ADMIN (compose grants it; a deployment that
# removes the grant is choosing the kernel default) this says so once and starts Caddy anyway.
routes=$(($(tune -4) + $(tune -6)))
if [ "$routes" -gt 0 ]; then
    echo "edge: initial congestion window set to $INITCWND segments on $routes route(s)" >&2
else
    echo "edge: initial congestion window left at the kernel default (changing it needs NET_ADMIN)" >&2
fi

# Holding on to the capability is the one thing that does keep the edge down. Caddy faces the
# internet and has no use for NET_ADMIN, so it leaves the bounding set before Caddy exists and
# nothing Caddy runs, or is made to run, can get it back. The second pass above checks that it
# has gone.
if holds_net_admin; then
    exec setpriv --bounding-set -net_admin "$0" --net-admin-dropped "$@"
fi
exec "$@"
