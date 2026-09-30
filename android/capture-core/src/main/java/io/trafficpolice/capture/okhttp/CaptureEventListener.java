package io.trafficpolice.capture.okhttp;

import io.trafficpolice.capture.core.CaptureRuntime;
import io.trafficpolice.capture.core.Recorder;
import io.trafficpolice.capture.core.ThreadStack;
import io.trafficpolice.capture.core.Txn;
import java.io.IOException;
import java.net.InetAddress;
import java.net.InetSocketAddress;
import java.net.Proxy;
import java.util.List;
import okhttp3.Call;
import okhttp3.Connection;
import okhttp3.EventListener;
import okhttp3.Handshake;
import okhttp3.Protocol;
import okhttp3.Request;
import okhttp3.Response;

/**
 * Our per-call listener (ARCHITECTURE.md §4.2): {@code callStart} runs on the thread that called
 * {@code execute()} or {@code enqueue()}, so it records the real initiating thread and stack; the
 * other callbacks become timing marks (PROTOCOL.md §9). Callbacks can arrive on other threads
 * (OkHttp 5 fast fallback), so nothing here depends on the calling thread. Never throws.
 */
final class CaptureEventListener extends EventListener {
    private final CaptureRuntime rt;
    private final CallState state;

    CaptureEventListener(CaptureRuntime rt, CallState state) {
        this.rt = rt;
        this.state = state;
        state.hasListener = true;
    }

    private void mark(String name) {
        try {
            state.mark(name, rt.recorder().now());
        } catch (Throwable t) {
            rt.internalError("okhttp.listener", t);
        }
    }

    @Override
    public void callStart(Call call) {
        try {
            state.stack = ThreadStack.capture(ThreadStack.ORIGIN_CALL, rt.config().stackDepth);
        } catch (Throwable t) {
            rt.internalError("okhttp.callStart", t);
        }
        mark("call_start");
    }

    @Override
    public void dnsStart(Call call, String domainName) {
        mark("dns_start");
    }

    @Override
    public void dnsEnd(Call call, String domainName, List<InetAddress> inetAddressList) {
        mark("dns_end");
    }

    @Override
    public void connectStart(Call call, InetSocketAddress inetSocketAddress, Proxy proxy) {
        mark("connect_start");
    }

    @Override
    public void secureConnectStart(Call call) {
        mark("tls_start");
    }

    @Override
    public void secureConnectEnd(Call call, Handshake handshake) {
        mark("tls_end");
    }

    @Override
    public void connectEnd(Call call, InetSocketAddress inetSocketAddress, Proxy proxy, Protocol protocol) {
        mark("connect_end");
    }

    @Override
    public void connectionAcquired(Call call, Connection connection) {
        mark("conn_acquired");
    }

    @Override
    public void connectionReleased(Call call, Connection connection) {
        mark("conn_released");
    }

    @Override
    public void requestHeadersStart(Call call) {
        mark("req_headers_start");
    }

    @Override
    public void requestHeadersEnd(Call call, Request request) {
        mark("req_headers_end");
    }

    @Override
    public void requestBodyStart(Call call) {
        mark("req_body_start");
    }

    @Override
    public void requestBodyEnd(Call call, long byteCount) {
        mark("req_body_end");
    }

    @Override
    public void responseHeadersStart(Call call) {
        mark("resp_headers_start");
    }

    @Override
    public void responseHeadersEnd(Call call, Response response) {
        mark("resp_headers_end");
    }

    @Override
    public void responseBodyStart(Call call) {
        mark("resp_body_start");
    }

    @Override
    public void responseBodyEnd(Call call, long byteCount) {
        mark("resp_body_end");
    }

    @Override
    public void callEnd(Call call) {
        mark("call_end");
    }

    /**
     * A call can fail before any network interceptor runs (DNS, connect, TLS): then no
     * transaction exists yet, so one is made from the call's request to show the failure.
     */
    @Override
    public void callFailed(Call call, IOException ioe) {
        mark("call_end");
        try {
            if (state.everStarted()) {
                return; // the interceptor or the body tee reports it
            }
            Recorder rec = rt.recorder();
            if (!rec.recording()) {
                return;
            }
            Request request = call.request();
            CallState.Marks marks = state.takePending();
            Recorder.RequestInfo r = new Recorder.RequestInfo();
            r.call = state.callId;
            r.hop = state.nextHop();
            r.method = request.method();
            r.url = request.url().toString();
            r.headers = OkHttpCompat.headers(request.headers());
            r.clientKind = "okhttp";
            r.clientVersion = CaptureInterceptor.okhttpVersion();
            r.stack = state.stack;
            r.hasBody = request.body() != null;
            r.markNames = marks.names;
            r.markTimes = marks.times;
            Txn txn = rec.start(r);
            state.setCurrent(txn);
            txn.fail("connect", call.isCanceled(), ioe, null);
        } catch (Throwable t) {
            rt.internalError("okhttp.callFailed", t);
        }
    }
}
