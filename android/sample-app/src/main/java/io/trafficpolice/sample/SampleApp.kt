package io.trafficpolice.sample

import android.app.Application

class SampleApp : Application() {
    override fun onCreate() {
        super.onCreate()
        // The library starts itself in the main process. Other processes (here ":worker") start
        // it themselves; calling it in every process is harmless.
        Capture.start(this)
    }
}
