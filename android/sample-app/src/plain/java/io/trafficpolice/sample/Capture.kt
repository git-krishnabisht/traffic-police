package io.trafficpolice.sample

import android.content.Context
import java.net.HttpURLConnection
import okhttp3.OkHttpClient

/** This build has no traffic-police code at all: watch it with attach mode (`--mode attach`). */
object Capture {
    @Suppress("UNUSED_PARAMETER")
    fun start(context: Context) {}

    fun instrument(builder: OkHttpClient.Builder): OkHttpClient.Builder = builder

    fun wrap(connection: HttpURLConnection): HttpURLConnection = connection

    val label: String
        get() = "no traffic-police library (watch with attach mode)"
}
