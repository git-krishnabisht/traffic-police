package io.trafficpolice.capture.core;

import java.util.ArrayDeque;
import java.util.Iterator;
import java.util.LinkedHashSet;
import java.util.List;

/**
 * The bounded queue between app threads and the writer (ARCHITECTURE.md §4.5). App threads
 * never wait: when the byte budget is exceeded the oldest events are dropped and counted, and
 * the writer reports them in a {@code dropped} event.
 */
final class EventQueue {
    private static final int MAX_DROPPED_TXNS = 100;

    private final ArrayDeque<Event> queue = new ArrayDeque<>();
    private final long maxBytes;
    private long bytes;
    private boolean woken;
    /** The writer waits with nothing to do; only then does an offer wake it (a system call). */
    private boolean idle;

    private long droppedEvents;
    private long droppedBytes;
    private final LinkedHashSet<Long> droppedTxns = new LinkedHashSet<>();
    private boolean droppedTxnsTruncated;

    EventQueue(long maxBytes) {
        this.maxBytes = maxBytes;
    }

    void offer(Event e) {
        e.queued = e.size(); // sized before taking the lock, which every app thread that records takes
        add(e);
    }

    private synchronized void add(Event e) {
        queue.addLast(e);
        bytes += e.queued;
        while (bytes > maxBytes && queue.size() > 1) {
            Event old = queue.pollFirst();
            bytes -= old.queued;
            droppedEvents++;
            droppedBytes += old.queued;
            if (old.txn != 0 && !droppedTxns.contains(old.txn)) {
                if (droppedTxns.size() < MAX_DROPPED_TXNS) {
                    droppedTxns.add(old.txn);
                } else {
                    droppedTxnsTruncated = true;
                }
            }
        }
        if (idle) {
            idle = false;
            notifyAll();
        }
    }

    /**
     * Moves every queued event to {@code out}, in order. With none queued, waits for one, or for
     * {@link #wake()}, as long as it takes: an idle app's writer does not wake up at all.
     */
    synchronized void drainTo(List<Event> out) throws InterruptedException {
        if (queue.isEmpty() && !woken) {
            idle = true;
            try {
                wait();
            } finally {
                idle = false;
            }
        }
        woken = false;
        Event e;
        while ((e = queue.pollFirst()) != null) {
            bytes -= e.queued;
            out.add(e);
        }
    }

    synchronized boolean isEmpty() {
        return queue.isEmpty();
    }

    /** What the queued events take, by their estimates (tests). */
    synchronized long bytes() {
        return bytes;
    }

    /** Makes a waiting {@link #drainTo} return so the writer can run its tasks. */
    synchronized void wake() {
        woken = true;
        notifyAll();
    }

    /** Drops since the last call, as a {@code dropped} event, or null. */
    synchronized Event.Dropped takeDropped(long now) {
        if (droppedEvents == 0) {
            return null;
        }
        long[] txns = new long[droppedTxns.size()];
        int i = 0;
        for (Iterator<Long> it = droppedTxns.iterator(); it.hasNext(); ) {
            txns[i++] = it.next();
        }
        Event.Dropped d = new Event.Dropped(now, droppedEvents, droppedBytes, txns, droppedTxnsTruncated);
        droppedEvents = 0;
        droppedBytes = 0;
        droppedTxns.clear();
        droppedTxnsTruncated = false;
        return d;
    }
}
