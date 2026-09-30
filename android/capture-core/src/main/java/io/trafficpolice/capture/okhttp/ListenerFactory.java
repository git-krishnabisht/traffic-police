package io.trafficpolice.capture.okhttp;

import io.trafficpolice.capture.core.CaptureRuntime;
import okhttp3.Call;
import okhttp3.EventListener;

/**
 * {@code TrafficPolice.eventListenerFactory(existing)}: our listener for every call, composed with
 * the app's own (the app's sees every callback first). Without a running runtime it returns the
 * app's listener unchanged.
 */
public final class ListenerFactory implements EventListener.Factory {
    private final EventListener.Factory app;

    public ListenerFactory(EventListener.Factory app) {
        this.app = app;
    }

    @Override
    public EventListener create(Call call) {
        EventListener appListener = app != null ? app.create(call) : null;
        CaptureRuntime rt = CaptureRuntime.current();
        if (rt == null) {
            return appListener != null ? appListener : EventListener.NONE;
        }
        try {
            CallState state = CallState.of(call, rt.recorder().newCallId());
            EventListener ours = new CaptureEventListener(rt, state);
            if (appListener == null || appListener == EventListener.NONE) {
                return ours;
            }
            return OkHttpCompat.compose(appListener, ours);
        } catch (Throwable t) {
            rt.internalError("okhttp.listenerFactory", t);
            return appListener != null ? appListener : EventListener.NONE;
        }
    }
}
