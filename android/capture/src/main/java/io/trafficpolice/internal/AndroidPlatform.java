package io.trafficpolice.internal;

import android.annotation.TargetApi;
import android.app.Application;
import android.content.Context;
import android.content.pm.ApplicationInfo;
import android.net.TrafficStats;
import android.os.Build;
import android.os.Process;
import android.os.SystemClock;
import android.util.Log;
import io.trafficpolice.capture.core.Platform;
import java.io.FileInputStream;
import java.io.IOException;

/** The runtime's view of Android: public SDK APIs only (no hidden APIs). */
final class AndroidPlatform implements Platform {
    private final AppInfo app;
    private final DeviceInfo device;

    AndroidPlatform(Context context) {
        ApplicationInfo info = context.getApplicationInfo();
        this.app = new AppInfo(context.getPackageName(), processName(), Process.myPid(), Process.myUid(),
                (info.flags & ApplicationInfo.FLAG_DEBUGGABLE) != 0);
        this.device = new DeviceInfo(Build.VERSION.SDK_INT, Build.VERSION.RELEASE, Build.MANUFACTURER, Build.MODEL,
                processAbi(), Build.SUPPORTED_ABIS);
    }

    private static String processName() {
        if (Build.VERSION.SDK_INT >= 28) {
            String name = processNameP();
            if (name != null) {
                return name;
            }
        }
        // before API 28: the kernel's copy of argv[0]
        byte[] buf = new byte[256];
        try (FileInputStream in = new FileInputStream("/proc/self/cmdline")) {
            int n = in.read(buf);
            int end = 0;
            while (end < n && buf[end] != 0) {
                end++;
            }
            if (end > 0) {
                return new String(buf, 0, end, "UTF-8");
            }
        } catch (IOException ignored) {
            // fall through
        }
        return "pid-" + Process.myPid();
    }

    @TargetApi(28)
    private static String processNameP() {
        return Application.getProcessName();
    }

    /** The ABI this process runs as (a 32-bit app on a 64-bit device runs a 32-bit ABI). */
    private static String processAbi() {
        if (Build.VERSION.SDK_INT >= 23) {
            String[] abis = is64Bit() ? Build.SUPPORTED_64_BIT_ABIS : Build.SUPPORTED_32_BIT_ABIS;
            if (abis.length > 0) {
                return abis[0];
            }
        }
        return Build.SUPPORTED_ABIS.length > 0 ? Build.SUPPORTED_ABIS[0] : null;
    }

    @TargetApi(23)
    private static boolean is64Bit() {
        return Process.is64Bit();
    }

    @Override
    public long nanoTime() {
        return SystemClock.elapsedRealtimeNanos();
    }

    @Override
    public long wallMillis() {
        return System.currentTimeMillis();
    }

    @Override
    public long[] trafficCounters() {
        long rx = TrafficStats.getUidRxBytes(app.uid);
        long tx = TrafficStats.getUidTxBytes(app.uid);
        if (rx == TrafficStats.UNSUPPORTED || tx == TrafficStats.UNSUPPORTED) {
            return null;
        }
        return new long[] {rx, tx};
    }

    @Override
    public AppInfo app() {
        return app;
    }

    @Override
    public DeviceInfo device() {
        return device;
    }

    @Override
    public void log(int level, String message, Throwable error) {
        if (level >= LOG_WARN) {
            Log.w(AndroidRuntime.TAG, message, error);
        } else if (level >= LOG_INFO) {
            Log.i(AndroidRuntime.TAG, message, error);
        } else {
            Log.d(AndroidRuntime.TAG, message, error);
        }
    }
}
