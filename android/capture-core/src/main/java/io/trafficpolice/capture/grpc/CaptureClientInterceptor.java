package io.trafficpolice.capture.grpc;

import io.grpc.CallOptions;
import io.grpc.Channel;
import io.grpc.ClientCall;
import io.grpc.ClientInterceptor;
import io.grpc.ClientStreamTracer;
import io.grpc.Metadata;
import io.grpc.MethodDescriptor;
import io.trafficpolice.capture.core.CaptureRuntime;

/**
 * gRPC calls (ARCHITECTURE.md §4.9): the interceptor that captures them. Added first to a
 * channel's interceptors it runs last, next to the transport, so it sees what the app's other
 * interceptors added. Library mode: {@code TrafficPolice.grpcInterceptor()}. Attach mode: the hook
 * on the channel builders' {@code getEffectiveInterceptors}, and the one on the stubs'
 * {@code getChannel} for channels built before the attach ({@link GrpcHooks}).
 *
 * <p>Compiled against grpc-api 1.21.0, the oldest version it supports, so it uses nothing newer;
 * {@link GrpcCall} reads what 1.40 added by name.
 */
public final class CaptureClientInterceptor implements ClientInterceptor {
    public static final CaptureClientInterceptor INSTANCE = new CaptureClientInterceptor();

    /**
     * Passed down with each call: when several of ours see one call (the library's and an attach
     * hook's, or a stub's channel above a channel that has one), only the innermost captures it.
     */
    static final CallOptions.Key<Claim> CLAIM = CallOptions.Key.create("io.trafficpolice.grpc.claim");

    /** One interceptor's hold on a call: who captures it, and the tracers it put on its streams. */
    static final class Claim extends ClientStreamTracer.Factory {
        volatile boolean claimedBelow;
        volatile GrpcCall.State<?, ?> state;

        @Override
        public ClientStreamTracer newClientStreamTracer(ClientStreamTracer.StreamInfo info, Metadata headers) {
            GrpcCall.State<?, ?> s = state;
            if (s == null) {
                // another interceptor of ours captures this call
                return new ClientStreamTracer() {};
            }
            return s.newTracer(info, headers);
        }
    }

    private CaptureClientInterceptor() {}

    @Override
    public <Q, R> ClientCall<Q, R> interceptCall(MethodDescriptor<Q, R> method, CallOptions options, Channel next) {
        CaptureRuntime rt = CaptureRuntime.current();
        if (rt == null || !rt.recorder().recording()) {
            return next.newCall(method, options);
        }
        Claim outer;
        Claim mine = new Claim();
        CallOptions withOurs;
        try {
            outer = options.getOption(CLAIM);
            withOurs = options.withOption(CLAIM, mine).withStreamTracerFactory(mine);
        } catch (Throwable t) {
            rt.internalError("grpc.interceptCall", t);
            return next.newCall(method, options);
        }
        ClientCall<Q, R> call = next.newCall(method, withOurs);
        if (outer != null) {
            outer.claimedBelow = true;
        }
        if (mine.claimedBelow) {
            return call;
        }
        try {
            GrpcCall.State<Q, R> state = new GrpcCall.State<>(rt, method, options, next.authority());
            mine.state = state;
            return new GrpcCall<>(call, state);
        } catch (Throwable t) {
            rt.internalError("grpc.interceptCall", t);
            return call;
        }
    }
}
