/* Zink's own presentation for OpenGL programs on Wayland.
 *
 * Mesa's EGL on Wayland asks the compositor for the GPU's DRM device, which Android doesn't give
 * apps. Without one it tries again in software mode, and drops Zink (OpenGL on Vulkan) on the way
 * even when MESA_LOADER_DRIVER_OVERRIDE=zink asked for it, so programs draw with llvmpipe on the
 * CPU. In software mode with Zink asked for, though, it takes Zink with its own presentation
 * through Vulkan's window system (kopper), which works here: Turnip draws on the GPU and the
 * frames reach the compositor like any Vulkan program's.
 *
 * Software mode is LIBGL_ALWAYS_SOFTWARE, which Zink reads too, and then looks for a CPU Vulkan
 * device instead of the GPU. So this library, preloaded into the programs that should draw with
 * Zink (see GPU_HELPER in src/android/proot/setup.rs), says it is set to Mesa's EGL alone, and
 * only where Zink was asked for. Everything else gets the environment as it is.
 *
 * Built by scripts/build-guest-libs.sh into assets/guest/.
 */
#define _GNU_SOURCE
#include <dlfcn.h>
#include <stddef.h>
#include <string.h>
#include <sys/auxv.h>

#define EXPORT __attribute__((visibility("default")))

extern char **environ;

/* The variable's value, as glibc's getenv(3) finds it. Not glibc's own, which the dynamic linker
 * would hand back as ours.  */
static char *lookup(const char *name)
{
    size_t length = strlen(name);
    for (char **entry = environ; entry && *entry; entry++) {
        if (strncmp(*entry, name, length) == 0 && (*entry)[length] == '=')
            return *entry + length + 1;
    }
    return NULL;
}

/* Whether the code at @address is Mesa's EGL's.  */
static int in_mesa_egl(const void *address)
{
    Dl_info info;
    return dladdr(address, &info) && info.dli_fname && strstr(info.dli_fname, "/libEGL_mesa.so");
}

static char *answer(const char *name, const void *caller)
{
    char *value = lookup(name);
    if (value || strcmp(name, "LIBGL_ALWAYS_SOFTWARE") != 0 || !in_mesa_egl(caller))
        return value;
    const char *driver = lookup("MESA_LOADER_DRIVER_OVERRIDE");
    return driver && strcmp(driver, "zink") == 0 ? "1" : NULL;
}

EXPORT char *getenv(const char *name)
{
    return answer(name, __builtin_return_address(0));
}

EXPORT char *secure_getenv(const char *name)
{
    if (getauxval(AT_SECURE))
        return NULL;
    return answer(name, __builtin_return_address(0));
}
