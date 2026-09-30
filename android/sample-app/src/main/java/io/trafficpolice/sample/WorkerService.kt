package io.trafficpolice.sample

import android.app.Service
import android.content.Intent
import android.os.IBinder
import okhttp3.OkHttpClient
import okhttp3.Request

/** Runs in the ":worker" process and makes one request, so that process shows up on its own. */
class WorkerService : Service() {
    override fun onBind(intent: Intent?): IBinder? = null

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        val port = intent?.getIntExtra("port", 0) ?: 0
        Thread({
            try {
                val client = Capture.instrument(OkHttpClient.Builder()).build()
                client.newCall(Request.Builder().url("http://127.0.0.1:$port/worker/ping").build()).execute().close()
            } catch (e: Exception) {
                android.util.Log.w("TrafficPoliceSample", "worker request failed", e)
            }
            stopSelf(startId)
        }, "worker-sync").start()
        return START_NOT_STICKY
    }
}
