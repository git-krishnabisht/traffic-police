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

    synchronized void offer(Event e) {
        queue.addLast(e);
        bytes += e.size();
        while (bytes > maxBytes && queue.size() > 1) {
            Event old = queue.pollFirst();
            bytes -= old.size();
            droppedEvents++;
            droppedBytes += old.size();
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
     * Moves every queued event to {@code out}, in order. With none queued, waits up to
     * {@code timeoutMillis} for one, or for {@link #wake()}.
     */
    synchronized void drainTo(List<Event> out, long timeoutMillis) throws InterruptedException {
        if (queue.isEmpty() && !woken) {
            idle = true;
            try {
                wait(timeoutMillis);
            } finally {
                idle = false;
            }
        }
        woken = false;
        Event e;
        while ((e = queue.pollFirst()) != null) {
            bytes -= e.size();
            out.add(e);
        }
    }

    synchronized boolean isEmpty() {
        return queue.isEmpty();
    }

    /** Makes a waiting {@link #poll} return so the writer can run its tasks. */
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
