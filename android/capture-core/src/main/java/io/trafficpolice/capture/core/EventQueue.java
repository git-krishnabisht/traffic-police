package io.trafficpolice.capture.core;

import java.util.ArrayDeque;
import java.util.ArrayList;
import java.util.Iterator;
import java.util.LinkedHashSet;
import java.util.List;

/**
 * The bounded queue between app threads and the writer (ARCHITECTURE.md §4.5). App threads
 * never wait: when the byte budget is exceeded the oldest events are dropped and counted, and
 * the writer reports them in a {@code dropped} event. It also keeps the bodies that gathered
 * short writes (Txn.Body), so that the writer sends those bytes when they are due.
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

    /** Bodies holding gathered bytes, all due when the first of them is (by the writer's clock). */
    private final ArrayList<Txn.Body> gathering = new ArrayList<>();
    private long gatheringDue;

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
     * A body gathered bytes (Txn.Body.GATHER_NS): they are due for the writer that long after the
     * first body on the list started gathering. A writer that waits without a timeout is woken to
     * take one.
     */
    synchronized void gathering(Txn.Body body) {
        gathering.add(body);
        if (gathering.size() == 1) {
            gatheringDue = System.nanoTime() + Txn.Body.GATHER_NS;
            if (idle) {
                notifyAll();
            }
        }
    }

    /**
     * Moves every queued event to {@code out}, in order, and the bodies whose gathered bytes are
     * due to {@code due}. With no event queued, waits for one, for {@link #wake()}, or until the
     * gathered bytes are due; with none gathered, as long as it takes: an idle app's writer does
     * not wake up at all.
     */
    synchronized void drainTo(List<Event> out, List<Txn.Body> due) throws InterruptedException {
        if (queue.isEmpty() && !woken) {
            long millis = 0; // no timeout
            if (!gathering.isEmpty()) {
                millis = (gatheringDue - System.nanoTime() + 999_999) / 1_000_000;
            }
            if (gathering.isEmpty() || millis > 0) {
                idle = true;
                try {
                    wait(millis);
                } finally {
                    idle = false;
                }
            }
        }
        woken = false;
        if (!gathering.isEmpty() && System.nanoTime() - gatheringDue >= 0) {
            due.addAll(gathering);
            gathering.clear();
        }
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
