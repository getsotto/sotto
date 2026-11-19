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
# it on the host changes nothing for a published port. It is set on every route rather than only
# the default one, so it holds whichever route the replies leave by.
#
# Tuning must never keep the edge down. Without NET_ADMIN (compose grants it; a deployment that
# removes the grant is choosing the kernel default) this says so once and starts Caddy anyway.
INITCWND=40

tuned=0
if routes=$(ip -4 route show 2>/dev/null); then
    tuned=1
    # Each line is a complete route as `ip route change` wants it back, less any earlier window.
    echo "$routes" | sed 's/ initcwnd [0-9]*//' | while read -r route; do
        # shellcheck disable=SC2086 # the route is a list of arguments, and must split into them
        ip -4 route change $route initcwnd "$INITCWND" 2>/dev/null || exit 1
    done || tuned=0
fi
if [ "$tuned" = 1 ]; then
    echo "edge: initial congestion window set to $INITCWND segments" >&2
else
    echo "edge: initial congestion window left at the kernel default (changing it needs NET_ADMIN)" >&2
fi

# Caddy faces the internet and has no use for NET_ADMIN, so it is removed from the bounding set
# before Caddy exists: nothing it runs, or is made to run, can get it back.
if setpriv --bounding-set -net_admin true 2>/dev/null; then
    exec setpriv --bounding-set -net_admin "$@"
fi
echo "edge: could not drop NET_ADMIN from the bounding set; starting without dropping it" >&2
exec "$@"
