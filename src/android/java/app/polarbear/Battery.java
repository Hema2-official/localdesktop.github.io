package app.polarbear;

import android.content.BroadcastReceiver;
import android.content.Context;
import android.content.Intent;
import android.content.IntentFilter;
import android.os.BatteryManager;
import android.os.Build;
import android.os.PowerManager;
import java.time.Duration;

/**
 * Android's battery, for the desktop's battery widget (src/android/battery.rs, src/core/upower.rs).
 *
 * Android sends a sticky broadcast with every change of the battery's level, plug, temperature or
 * voltage. Only the level and the plug matter to the desktop, so only their changes wake the
 * guest link (Native.onBatteryChanged), which then reads the rest.
 */
final class Battery {
    /** BatteryManager.EXTRA_CYCLE_COUNT, from Android 14. */
    private static final String EXTRA_CYCLE_COUNT = "android.os.extra.CYCLE_COUNT";

    private static BroadcastReceiver receiver;
    private static Context context;
    /** The last broadcast, and what of it the desktop has been told about. */
    private static Intent last;
    private static String told;
    /** What the battery held new (µAh), Long.MIN_VALUE if the phone doesn't say; 0 before a look. */
    private static long design;

    private Battery() {}

    /** Start or stop listening. */
    static synchronized void watch(Context activity, boolean on) {
        if (on == (receiver != null)) {
            return;
        }
        if (!on) {
            context.unregisterReceiver(receiver);
            receiver = null;
            return;
        }
        context = activity.getApplicationContext();
        receiver = new BroadcastReceiver() {
            @Override
            public void onReceive(Context context, Intent intent) {
                changed(intent);
            }
        };
        IntentFilter filter = new IntentFilter(Intent.ACTION_BATTERY_CHANGED);
        // The broadcast is sticky: registering returns the latest one.
        last = Build.VERSION.SDK_INT >= 33
                ? context.registerReceiver(receiver, filter, Context.RECEIVER_NOT_EXPORTED)
                : context.registerReceiver(receiver, filter);
        told = summary(last);
    }

    private static String summary(Intent intent) {
        if (intent == null) {
            return null;
        }
        return intent.getBooleanExtra(BatteryManager.EXTRA_PRESENT, false)
                + " " + intent.getIntExtra(BatteryManager.EXTRA_LEVEL, -1)
                + "/" + intent.getIntExtra(BatteryManager.EXTRA_SCALE, -1)
                + " " + intent.getIntExtra(BatteryManager.EXTRA_STATUS, -1)
                + " " + intent.getIntExtra(BatteryManager.EXTRA_PLUGGED, -1);
    }

    private static synchronized void changed(Intent intent) {
        last = intent;
        String summary = summary(intent);
        if (!summary.equals(told)) {
            told = summary;
            Native.onBatteryChanged();
        }
    }

    /**
     * The battery: present (1 or 0), level, scale, status, plug, voltage (mV), temperature (tenths
     * of °C), the charge left (µAh), the current (µA), the time until full and until empty (ms),
     * the charge cycles and what the battery held new (µAh), Long.MIN_VALUE for what Android
     * doesn't tell. Null before Android has told anything.
     */
    static synchronized long[] read() {
        if (last == null) {
            return null;
        }
        Intent intent = last;
        BatteryManager manager = (BatteryManager) context.getSystemService(Context.BATTERY_SERVICE);
        long charge = manager.getLongProperty(BatteryManager.BATTERY_PROPERTY_CHARGE_COUNTER);
        long current = manager.getLongProperty(BatteryManager.BATTERY_PROPERTY_CURRENT_NOW);
        long untilFull = Build.VERSION.SDK_INT >= 28 ? manager.computeChargeTimeRemaining() : -1;
        long untilEmpty = -1;
        if (Build.VERSION.SDK_INT >= 31) {
            PowerManager power = (PowerManager) context.getSystemService(Context.POWER_SERVICE);
            Duration prediction = power.getBatteryDischargePrediction();
            untilEmpty = prediction == null ? -1 : prediction.toMillis();
        }
        int cycles = intent.getIntExtra(EXTRA_CYCLE_COUNT, -1);
        if (design == 0) {
            design = designCapacity();
        }
        return new long[] {
            intent.getBooleanExtra(BatteryManager.EXTRA_PRESENT, false) ? 1 : 0,
            intent.getIntExtra(BatteryManager.EXTRA_LEVEL, 0),
            intent.getIntExtra(BatteryManager.EXTRA_SCALE, 100),
            intent.getIntExtra(BatteryManager.EXTRA_STATUS, BatteryManager.BATTERY_STATUS_UNKNOWN),
            intent.getIntExtra(BatteryManager.EXTRA_PLUGGED, 0),
            intent.getIntExtra(BatteryManager.EXTRA_VOLTAGE, 0),
            intent.getIntExtra(BatteryManager.EXTRA_TEMPERATURE, 0),
            charge,
            current,
            untilFull >= 0 ? untilFull : Long.MIN_VALUE,
            untilEmpty > 0 ? untilEmpty : Long.MIN_VALUE,
            cycles >= 0 ? cycles : Long.MIN_VALUE,
            design,
        };
    }

    /**
     * What the battery held new, in µAh: the capacity in the phone's power profile (the one
     * `dumpsys batterystats` shows), which the SDK keeps to itself.
     */
    private static long designCapacity() {
        try {
            Class<?> profile = Class.forName("com.android.internal.os.PowerProfile");
            Object instance = profile.getConstructor(Context.class).newInstance(context);
            double mah = (Double) profile.getMethod("getBatteryCapacity").invoke(instance);
            // AOSP's own profile says 1000 mAh, which a phone without one of its own keeps.
            return mah > 1000 ? (long) (mah * 1000) : Long.MIN_VALUE;
        } catch (ReflectiveOperationException | RuntimeException | LinkageError e) {
            return Long.MIN_VALUE;
        }
    }

    /** The battery's technology ("Li-ion"), and the phone's maker and model, which name it. */
    static synchronized String[] describe() {
        String technology =
                last == null ? null : last.getStringExtra(BatteryManager.EXTRA_TECHNOLOGY);
        return new String[] {technology, Build.MANUFACTURER, Build.MODEL};
    }
}
