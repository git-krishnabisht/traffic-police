package io.trafficpolice.capture.okhttp;

import java.io.IOException;
import java.net.InetAddress;
import java.net.InetSocketAddress;
import java.net.Proxy;
import java.util.List;
import okhttp3.Call;
import okhttp3.Connection;
import okhttp3.Dispatcher;
import okhttp3.EventListener;
import okhttp3.Handshake;
import okhttp3.HttpUrl;
import okhttp3.Protocol;
import okhttp3.Request;
import okhttp3.Response;

/**
 * Sends every callback to the app's listener first, then to ours. Compiled against OkHttp 5.5.0
 * so it overrides all 33 callbacks known up to 5.5 (docs/research/05-okhttp-okio.md §3e): a
 * wrapper compiled against an older API would silently stop forwarding newer callbacks to the
 * app's listener. Callbacks an older OkHttp does not have are simply never called. It must not
 * declare {@code plus()}, which is final from 5.3. Used below 5.3; newer versions use
 * {@code EventListener.plus()}.
 */
final class ForwardingEventListener extends EventListener {
    private final EventListener first;
    private final EventListener second;

    ForwardingEventListener(EventListener first, EventListener second) {
        this.first = first;
        this.second = second;
    }

    @Override
    public void dispatcherQueueStart(Call call, Dispatcher dispatcher) {
        first.dispatcherQueueStart(call, dispatcher);
        second.dispatcherQueueStart(call, dispatcher);
    }

    @Override
    public void dispatcherQueueEnd(Call call, Dispatcher dispatcher) {
        first.dispatcherQueueEnd(call, dispatcher);
        second.dispatcherQueueEnd(call, dispatcher);
    }

    @Override
    public void callStart(Call call) {
        first.callStart(call);
        second.callStart(call);
    }

    @Override
    public void proxySelectStart(Call call, HttpUrl url) {
        first.proxySelectStart(call, url);
        second.proxySelectStart(call, url);
    }

    @Override
    public void proxySelectEnd(Call call, HttpUrl url, List<Proxy> proxies) {
        first.proxySelectEnd(call, url, proxies);
        second.proxySelectEnd(call, url, proxies);
    }

    @Override
    public void dnsStart(Call call, String domainName) {
        first.dnsStart(call, domainName);
        second.dnsStart(call, domainName);
    }

    @Override
    public void dnsEnd(Call call, String domainName, List<InetAddress> inetAddressList) {
        first.dnsEnd(call, domainName, inetAddressList);
        second.dnsEnd(call, domainName, inetAddressList);
    }

    @Override
    public void connectStart(Call call, InetSocketAddress inetSocketAddress, Proxy proxy) {
        first.connectStart(call, inetSocketAddress, proxy);
        second.connectStart(call, inetSocketAddress, proxy);
    }

    @Override
    public void secureConnectStart(Call call) {
        first.secureConnectStart(call);
        second.secureConnectStart(call);
    }

    @Override
    public void secureConnectEnd(Call call, Handshake handshake) {
        first.secureConnectEnd(call, handshake);
        second.secureConnectEnd(call, handshake);
    }

    @Override
    public void connectEnd(Call call, InetSocketAddress inetSocketAddress, Proxy proxy, Protocol protocol) {
        first.connectEnd(call, inetSocketAddress, proxy, protocol);
        second.connectEnd(call, inetSocketAddress, proxy, protocol);
    }

    @Override
    public void connectFailed(Call call, InetSocketAddress inetSocketAddress, Proxy proxy, Protocol protocol,
            IOException ioe) {
        first.connectFailed(call, inetSocketAddress, proxy, protocol, ioe);
        second.connectFailed(call, inetSocketAddress, proxy, protocol, ioe);
    }

    @Override
    public void connectionAcquired(Call call, Connection connection) {
        first.connectionAcquired(call, connection);
        second.connectionAcquired(call, connection);
    }

    @Override
    public void connectionReleased(Call call, Connection connection) {
        first.connectionReleased(call, connection);
        second.connectionReleased(call, connection);
    }

    @Override
    public void requestHeadersStart(Call call) {
        first.requestHeadersStart(call);
        second.requestHeadersStart(call);
    }

    @Override
    public void requestHeadersEnd(Call call, Request request) {
        first.requestHeadersEnd(call, request);
        second.requestHeadersEnd(call, request);
    }

    @Override
    public void requestBodyStart(Call call) {
        first.requestBodyStart(call);
        second.requestBodyStart(call);
    }

    @Override
    public void requestBodyEnd(Call call, long byteCount) {
        first.requestBodyEnd(call, byteCount);
        second.requestBodyEnd(call, byteCount);
    }

    @Override
    public void requestFailed(Call call, IOException ioe) {
        first.requestFailed(call, ioe);
        second.requestFailed(call, ioe);
    }

    @Override
    public void responseHeadersStart(Call call) {
        first.responseHeadersStart(call);
        second.responseHeadersStart(call);
    }

    @Override
    public void responseHeadersEnd(Call call, Response response) {
        first.responseHeadersEnd(call, response);
        second.responseHeadersEnd(call, response);
    }

    @Override
    public void responseBodyStart(Call call) {
        first.responseBodyStart(call);
        second.responseBodyStart(call);
    }

    @Override
    public void responseBodyEnd(Call call, long byteCount) {
        first.responseBodyEnd(call, byteCount);
        second.responseBodyEnd(call, byteCount);
    }

    @Override
    public void responseFailed(Call call, IOException ioe) {
        first.responseFailed(call, ioe);
        second.responseFailed(call, ioe);
    }

    @Override
    public void callEnd(Call call) {
        first.callEnd(call);
        second.callEnd(call);
    }

    @Override
    public void callFailed(Call call, IOException ioe) {
        first.callFailed(call, ioe);
        second.callFailed(call, ioe);
    }

    @Override
    public void canceled(Call call) {
        first.canceled(call);
        second.canceled(call);
    }

    @Override
    public void satisfactionFailure(Call call, Response response) {
        first.satisfactionFailure(call, response);
        second.satisfactionFailure(call, response);
    }

    @Override
    public void cacheHit(Call call, Response response) {
        first.cacheHit(call, response);
        second.cacheHit(call, response);
    }

    @Override
    public void cacheMiss(Call call) {
        first.cacheMiss(call);
        second.cacheMiss(call);
    }

    @Override
    public void cacheConditionalHit(Call call, Response cachedResponse) {
        first.cacheConditionalHit(call, cachedResponse);
        second.cacheConditionalHit(call, cachedResponse);
    }

    @Override
    public void retryDecision(Call call, IOException exception, boolean retry) {
        first.retryDecision(call, exception, retry);
        second.retryDecision(call, exception, retry);
    }

    @Override
    public void followUpDecision(Call call, Response networkResponse, Request nextRequest) {
        first.followUpDecision(call, networkResponse, nextRequest);
        second.followUpDecision(call, networkResponse, nextRequest);
    }
}
