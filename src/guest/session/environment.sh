export PIPEWIRE_RUNTIME_DIR=@PIPEWIRE_GUEST_RUNTIME_DIR@
export PULSE_SERVER=@PULSE_GUEST_SERVER@
# A private runtime directory per user, like logind would provide, so sockets, locks and dconf
# state that another user's session left in the shared /tmp don't get in the way.
if [ -z "${XDG_RUNTIME_DIR:-}" ] || [ "$XDG_RUNTIME_DIR" = /tmp ]; then
    XDG_RUNTIME_DIR=/tmp/runtime-$(id -u)
fi
mkdir -p "$XDG_RUNTIME_DIR" && chmod 700 "$XDG_RUNTIME_DIR"
export XDG_RUNTIME_DIR
# Local Desktop's own compositor socket stays in /tmp.
case "${WAYLAND_DISPLAY:=wayland-0}" in
    /*) ;;
    *) WAYLAND_DISPLAY=/tmp/$WAYLAND_DISPLAY ;;
esac
export WAYLAND_DISPLAY
# Electron adds --no-sandbox when this is set; Android has no user namespaces for it to use.
export ELECTRON_DISABLE_SANDBOX=1
# The compositors here only take shared-memory buffers (there's no GPU render node), so Vulkan
# apps have to present through them.
export MESA_VK_WSI_DEBUG=sw
# Firefox's main process can't reopen a memfd read-only through /proc/self/fd (Android's SELinux),
# so it shares memory through /dev/shm instead; its content processes still insist on memfd seals
# and crashed on every page ("Shared memory PlatformHandle is not safe to map").
export MOZ_SHM_NO_SEALS=1
@TZ_FROM_LOCALTIME@
