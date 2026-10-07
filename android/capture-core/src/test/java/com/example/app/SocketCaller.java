package com.example.app;

import io.trafficpolice.capture.okhttp.OkHttpHooks;
import io.trafficpolice.capture.okhttp.WebSockets;
import okhttp3.OkHttpClient;
import okhttp3.Request;
import okhttp3.WebSocket;
import okhttp3.WebSocketListener;

/** An app's code opening sockets: outside the runtime's packages, so its frame is the call site. */
public final class SocketCaller {
    private SocketCaller() {}

    public static WebSocket library(OkHttpClient client, Request request, WebSocketListener listener) {
        return WebSockets.newWebSocket(client, request, listener);
    }

    /** As in attach mode: the exit hook wraps what newWebSocket returned. */
    public static WebSocket attached(OkHttpClient client, Request request, WebSocketListener listener) {
        return OkHttpHooks.newWebSocket(client.newWebSocket(request, listener));
    }
}
