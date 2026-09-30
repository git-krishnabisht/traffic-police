package io.trafficpolice.internal;

import android.content.Context;
import android.content.pm.ApplicationInfo;
import android.util.Log;
import io.trafficpolice.capture.core.CaptureRuntime;
import io.trafficpolice.capture.core.SocketNames;
import io.trafficpolice.capture.okhttp.OkHttpDetect;

/** Starts the runtime in an Android process (library mode). */
public final class AndroidRuntime {
    static final String TAG = "TrafficPolice";

    private AndroidRuntime() {}

    public static synchronized void start(Context context) {
        if (context == null || CaptureRuntime.current() != null) {
            return;
        }
        Context app = context.getApplicationContext() != null ? context.getApplicationContext() : context;
        ApplicationInfo info = app.getApplicationInfo();
        if ((info.flags & ApplicationInfo.FLAG_DEBUGGABLE) == 0) {
            // never capture in a non-debuggable build (ARCHITECTURE.md §7)
            Log.i(TAG, "not starting: " + app.getPackageName() + " is not debuggable");
            return;
        }
        AndroidPlatform platform = new AndroidPlatform(app);
        CaptureRuntime.Options options = new CaptureRuntime.Options();
        options.mode = "library";
        boolean okhttp = OkHttpDetect.present();
        boolean supported = okhttp && OkHttpDetect.supported();
        options.okhttp = supported;
        options.clients.put("okhttp", okhttp ? OkHttpDetect.version() : null);
        CaptureRuntime rt = CaptureRuntime.start(platform, options);
        if (okhttp && !supported) {
            rt.diag("warn", "okhttp_unsupported_version",
                    "OkHttp " + OkHttpDetect.version() + " is older than 3.9 and is not captured", null);
        }
        String name = SocketNames.forProcess(app.getPackageName(), platform.app().pid);
        LocalSocketServer.start(rt, name);
        Log.i(TAG, "capturing in " + platform.app().processName + "; socket @" + name);
    }
}
