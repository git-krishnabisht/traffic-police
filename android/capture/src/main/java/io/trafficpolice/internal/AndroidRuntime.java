package io.trafficpolice.internal;

import android.content.Context;
import android.content.pm.ApplicationInfo;
import android.util.Log;
import io.trafficpolice.capture.core.CaptureRuntime;
import io.trafficpolice.capture.core.SocketNames;
import io.trafficpolice.capture.grpc.GrpcDetect;
import io.trafficpolice.capture.okhttp.OkHttpDetect;
import java.util.Map;

/** Starts the runtime in an Android process (library mode). */
public final class AndroidRuntime {
    static final String TAG = "TrafficPolice";

    /** What the app asked the replay ring to keep (TrafficPolice.buffer); -1 until it asks. */
    private static int bufferTxns = -1;
    private static long bufferBytes = -1;

    private AndroidRuntime() {}

    /** Applies to the runtime running in this process, and to one started in it later. */
    public static synchronized void buffer(int txns, long bytes) {
        bufferTxns = Math.max(0, txns);
        bufferBytes = Math.max(0, bytes);
        CaptureRuntime rt = CaptureRuntime.current();
        if (rt != null) {
            rt.buffer(bufferTxns, bufferBytes);
        }
    }

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
        if (bufferTxns >= 0) {
            options.ringTxns = bufferTxns;
            options.ringBytes = bufferBytes;
        }
        final boolean okhttp = OkHttpDetect.present();
        boolean supported = okhttp && OkHttpDetect.supported();
        options.okhttp = supported;
        final boolean grpc = grpcPresent();
        // the versions are read when a host connects, not here on the main thread before
        // Application.onCreate: reading gRPC's initializes io.grpc.internal.GrpcUtil (and OkHttp's
        // loads its classes); the hello reports the same
        options.lateClients = new CaptureRuntime.Clients() {
            @Override
            public void clients(Map<String, String> into) {
                into.put("okhttp", okhttp ? OkHttpDetect.version() : null);
                if (grpc) {
                    into.put("grpc", GrpcDetect.version());
                }
            }
        };
        CaptureRuntime rt = CaptureRuntime.start(platform, options);
        if (okhttp && !supported) {
            rt.diag("warn", "okhttp_unsupported_version",
                    "OkHttp " + OkHttpDetect.version() + " is older than 3.9 and is not captured", null);
        }
        String name = SocketNames.forProcess(app.getPackageName(), platform.app().pid);
        LocalSocketServer.start(rt, name);
        Log.i(TAG, "capturing in " + platform.app().processName + "; socket @" + name);
    }

    /** Whether grpc-java is in the app (its API; the library links to nothing it does not have). */
    private static boolean grpcPresent() {
        try {
            Class.forName("io.grpc.ClientInterceptor", false, AndroidRuntime.class.getClassLoader());
            return true;
        } catch (Throwable t) {
            return false;
        }
    }
}
