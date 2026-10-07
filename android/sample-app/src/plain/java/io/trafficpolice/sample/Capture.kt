package io.trafficpolice.sample

import android.content.Context
import java.net.HttpURLConnection
import okhttp3.OkHttpClient
import okhttp3.Request
import okhttp3.WebSocket
import okhttp3.WebSocketListener

/** This build has no traffic-police code at all: watch it with attach mode (`--mode attach`). */
object Capture {
    @Suppress("UNUSED_PARAMETER")
    fun start(context: Context) {}

    fun instrument(builder: OkHttpClient.Builder): OkHttpClient.Builder = builder

    fun wrap(connection: HttpURLConnection): HttpURLConnection = connection

    fun grpc(builder: io.grpc.ManagedChannelBuilder<*>): io.grpc.ManagedChannelBuilder<*> = builder

    fun newWebSocket(client: OkHttpClient, request: Request, listener: WebSocketListener): WebSocket =
        client.newWebSocket(request, listener)

    val label: String
        get() = "no traffic-police library (watch with attach mode)"
}
