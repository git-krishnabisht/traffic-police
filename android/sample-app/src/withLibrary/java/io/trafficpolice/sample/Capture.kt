package io.trafficpolice.sample

import android.content.Context
import io.trafficpolice.TrafficPolice
import java.net.HttpURLConnection
import okhttp3.OkHttpClient
import okhttp3.Request
import okhttp3.WebSocket
import okhttp3.WebSocketListener

/** The traffic-police library, in the builds that have it (debug: the real one; release: the no-op). */
object Capture {
    fun start(context: Context) = TrafficPolice.start(context)

    /** Our network interceptor and listener on a client, as the README shows. */
    fun instrument(builder: OkHttpClient.Builder): OkHttpClient.Builder = builder
        .addNetworkInterceptor(TrafficPolice.networkInterceptor())
        .eventListenerFactory(TrafficPolice.eventListenerFactory())

    fun wrap(connection: HttpURLConnection): HttpURLConnection = TrafficPolice.wrap(connection)

    /** gRPC: our interceptor first on the channel, so it sees what later interceptors add. */
    fun grpc(builder: io.grpc.ManagedChannelBuilder<*>): io.grpc.ManagedChannelBuilder<*> =
        builder.intercept(TrafficPolice.grpcInterceptor())

    /** A WebSocket and every message on it (interceptors never see a socket's messages). */
    fun newWebSocket(client: OkHttpClient, request: Request, listener: WebSocketListener): WebSocket =
        TrafficPolice.newWebSocket(client, request, listener)

    val label: String
        get() = if (TrafficPolice.isActive()) "capture: on (debug build)" else "capture: off"
}
