package io.trafficpolice.capture.core;

/**
 * Samples the app's whole-uid byte counters (what Android Studio's graph plots) and records a
 * {@code traffic} event whenever they change (PROTOCOL.md §7.1). The first sample is sent too, as
 * the host's baseline.
 */
final class TrafficSampler implements Runnable {
    private final CaptureRuntime rt;
    private final long intervalMillis;
    private final Thread thread;
    private volatile boolean running = true;

    TrafficSampler(CaptureRuntime rt, long intervalMillis) {
        this.rt = rt;
        this.intervalMillis = intervalMillis;
        this.thread = new Thread(this, "traffic-police-traffic");
        this.thread.setDaemon(true);
    }

    void start() {
        thread.start();
    }

    void stop() {
        running = false;
        thread.interrupt();
    }

    @Override
    public void run() {
        long prevTick = 0;
        long lastRx = -1;
        long lastTx = -1;
        while (running) {
            try {
                long[] c = rt.platform.trafficCounters();
                long now = rt.platform.nanoTime();
                if (c != null && (c[0] != lastRx || c[1] != lastTx)) {
                    rt.queue.offer(new Event.Traffic(now, c[0], c[1], lastRx < 0 ? 0 : prevTick));
                    lastRx = c[0];
                    lastTx = c[1];
                }
                prevTick = now;
                Thread.sleep(intervalMillis);
            } catch (InterruptedException e) {
                return;
            } catch (Throwable t) {
                rt.internalError("traffic", t);
                return;
            }
        }
    }
}
