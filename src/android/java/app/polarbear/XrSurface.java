package app.polarbear;

import android.hardware.HardwareBuffer;
import android.hardware.SyncFence;
import android.media.Image;
import android.media.ImageWriter;
import android.os.Handler;
import android.os.HandlerThread;
import android.view.Surface;
import java.io.IOException;
import java.time.Duration;
import java.util.HashMap;
import java.util.concurrent.atomic.AtomicInteger;

/**
 * The producer end of an OpenXR surface swapchain, whose buffers Linux renders immersive mode's
 * frames into (src/android/xr/frames.rs): the headset's compositor reads them from there, with no
 * copy in between. The app dequeues every buffer, lends them to Linux, queues each frame with the
 * time it shows from, and dequeues the buffers the compositor gives back.
 *
 * Calls come from the session's thread. The writer tells of buffers coming back on a thread of
 * its own, which only counts them, so the session's thread dequeues one only when it won't wait.
 */
final class XrSurface implements ImageWriter.OnImageReleasedListener {
    /** How long a buffer that came back may still be read by the compositor. */
    private static final Duration READ_TIMEOUT = Duration.ofMillis(100);

    private final HandlerThread thread = new HandlerThread("XrSurface");
    private final ImageWriter writer;
    private final AtomicInteger released = new AtomicInteger();
    /** The images the app dequeued and hasn't queued, by their buffers' ids. */
    private final HashMap<Long, Image> dequeued = new HashMap<Long, Image>();
    /** The image dequeued last, until it has its id. */
    private Image latest;

    XrSurface(Surface surface, int count, int width, int height, long usage) {
        writer = new ImageWriter.Builder(surface)
                .setMaxImages(count)
                .setWidthAndHeight(width, height)
                .setHardwareBufferFormat(HardwareBuffer.RGBA_8888)
                .setUsage(usage)
                .build();
        thread.start();
        writer.setOnImageReleasedListener(this, new Handler(thread.getLooper()));
    }

    @Override
    public void onImageReleased(ImageWriter writer) {
        released.incrementAndGet();
    }

    /** How many buffers came back since the last call; each can be dequeued without waiting. */
    int takeReleased() {
        return released.getAndSet(0);
    }

    /**
     * Dequeue a buffer, once the compositor is done reading it. The caller closes the returned
     * object, and gives the image its buffer's id with {@link #keep}.
     */
    HardwareBuffer dequeue() throws IOException {
        Image image = writer.dequeueInputImage();
        SyncFence fence = image.getFence();
        try {
            if (fence.isValid()) {
                fence.await(READ_TIMEOUT);
            }
        } finally {
            fence.close();
        }
        latest = image;
        return image.getHardwareBuffer();
    }

    /** Keep the image dequeued last by its buffer's id (AHardwareBuffer_getId) till it's queued. */
    void keep(long id) {
        dequeued.put(id, latest);
        latest = null;
    }

    /** Queue the buffer with this id, to show from `timestampNs` (CLOCK_MONOTONIC) on. */
    boolean queue(long id, long timestampNs) {
        Image image = dequeued.remove(id);
        if (image == null) {
            return false;
        }
        image.setTimestamp(timestampNs);
        writer.queueInputImage(image);
        return true;
    }

    void close() {
        for (Image image : dequeued.values()) {
            image.close();
        }
        dequeued.clear();
        if (latest != null) {
            latest.close();
            latest = null;
        }
        writer.close();
        thread.quitSafely();
    }
}
