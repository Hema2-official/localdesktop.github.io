if [ -n "$turnip" ] && [ -e @ZINK_EGL_LIB@ ]; then
    export MESA_LOADER_DRIVER_OVERRIDE=zink LIBGL_KOPPER_DRI2=1
    export LD_PRELOAD="@ZINK_EGL_LIB@${LD_PRELOAD:+:$LD_PRELOAD}"
fi
