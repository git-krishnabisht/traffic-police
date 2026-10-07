package io.trafficpolice.capture.grpc;

import io.grpc.Channel;
import io.grpc.ClientInterceptor;
import io.grpc.ClientInterceptors;
import java.util.ArrayList;
import java.util.List;

/**
 * What attach mode's gRPC hooks return (ARCHITECTURE.md §4.9). A channel builder's
 * {@code getEffectiveInterceptors} runs once per channel it builds, so channels built after the
 * attach get our interceptor where the library would put it. A stub's {@code getChannel} runs for
 * every call a generated stub makes, which reaches the channels built before the attach.
 */
public final class GrpcHooks {
    private GrpcHooks() {}

    /** The builder's interceptors with ours first (innermost: it runs last), unless it is there. */
    public static List<ClientInterceptor> effectiveInterceptors(List<ClientInterceptor> original) {
        if (original != null) {
            for (ClientInterceptor i : original) {
                // by name: the library's own interceptor, in an app that has both, counts too
                if (i != null && i.getClass().getName().equals(CaptureClientInterceptor.class.getName())) {
                    return original;
                }
            }
        }
        List<ClientInterceptor> out = new ArrayList<>(original == null ? 1 : original.size() + 1);
        out.add(CaptureClientInterceptor.INSTANCE);
        if (original != null) {
            out.addAll(original);
        }
        return out;
    }

    /**
     * The stub's channel with our interceptor on top. When the channel has ours too (built after
     * the attach), the inner one captures and this one stands aside.
     */
    public static Channel stubChannel(Channel original) {
        if (original == null) {
            return null;
        }
        return ClientInterceptors.intercept(original, CaptureClientInterceptor.INSTANCE);
    }
}
