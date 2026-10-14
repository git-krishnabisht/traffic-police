package io.trafficpolice.capture.core;

import java.util.ArrayDeque;
import java.util.ArrayList;
import java.util.Collections;
import java.util.Comparator;
import java.util.Iterator;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;

/**
 * Recent encoded frames, replayed to a host when it connects (PROTOCOL.md §6). Transactions are
 * kept whole and evicted oldest first when either limit is reached: their number, or the memory
 * they take (every frame's bytes, headers and stack frames included, plus what keeping each frame
 * and transaction costs). Diagnostics and traffic samples are kept separately, by count (a few
 * hundred small frames), so start-up diagnostics are always replayed.
 */
final class ReplayRing {
    static final int MAX_DIAG = 200;
    static final int MAX_TRAFFIC = 240;
    /**
     * What keeping a frame costs besides its bytes, on a 64-bit VM: its Entry, the array's header
     * and its slot in the list. A chunk of one body byte is a 40-byte frame and costs 104: until
     * 0.5.1 the budget counted only its body byte, so byte-at-a-time bodies took 100 times it.
     */
    static final int FRAME_OVERHEAD = 64;
    /** What keeping a transaction costs besides its frames: its map entry and key, list and sums. */
    static final int TXN_OVERHEAD = 200;

    static final class Entry {
        final long seq;
        final byte[] frame;

        Entry(long seq, byte[] frame) {
            this.seq = seq;
            this.frame = frame;
        }
    }

    private static final class TxnFrames {
        final List<Entry> frames = new ArrayList<>(8);
        long bytes = TXN_OVERHEAD;
        long bodyBytes;
    }

    private final int maxTxns;
    private final long maxBytes;
    private final LinkedHashMap<Long, TxnFrames> txns = new LinkedHashMap<>();
    private final ArrayDeque<Entry> diag = new ArrayDeque<>();
    private final ArrayDeque<Entry> traffic = new ArrayDeque<>();
    /** What the kept transactions cost: the budget's measure. */
    private long bytes;
    /** The body bytes among them, which hello reports. */
    private long bodyBytes;
    private long firstSeq;
    private long lastSeq;

    ReplayRing(int maxTxns, long maxBytes) {
        this.maxTxns = maxTxns;
        this.maxBytes = maxBytes;
    }

    synchronized void add(long seq, Event e, byte[] frame) {
        Entry entry = new Entry(seq, frame);
        lastSeq = seq;
        if (firstSeq == 0) {
            firstSeq = seq;
        }
        switch (e.ring()) {
            case Event.RING_TXN: {
                TxnFrames t = txns.get(e.txn);
                if (t == null) {
                    t = new TxnFrames();
                    txns.put(e.txn, t);
                    bytes += t.bytes;
                }
                t.frames.add(entry);
                long cost = frame.length + FRAME_OVERHEAD;
                t.bytes += cost;
                bytes += cost;
                t.bodyBytes += e.bodyBytes();
                bodyBytes += e.bodyBytes();
                evict();
                break;
            }
            case Event.RING_TRAFFIC:
                traffic.addLast(entry);
                if (traffic.size() > MAX_TRAFFIC) {
                    traffic.pollFirst();
                }
                break;
            default:
                diag.addLast(entry);
                if (diag.size() > MAX_DIAG) {
                    diag.pollFirst();
                }
        }
    }

    private void evict() {
        Iterator<Map.Entry<Long, TxnFrames>> it = txns.entrySet().iterator();
        while ((txns.size() > maxTxns || bytes > maxBytes) && it.hasNext()) {
            TxnFrames old = it.next().getValue();
            bytes -= old.bytes;
            bodyBytes -= old.bodyBytes;
            it.remove();
        }
    }

    /** What the kept transactions cost (tests). */
    synchronized long bytes() {
        return bytes;
    }

    /** Every kept frame with a seq above {@code afterSeq}, in seq order. */
    synchronized List<Entry> after(long afterSeq) {
        List<Entry> out = new ArrayList<>();
        for (TxnFrames t : txns.values()) {
            for (Entry e : t.frames) {
                if (e.seq > afterSeq) {
                    out.add(e);
                }
            }
        }
        for (Entry e : diag) {
            if (e.seq > afterSeq) {
                out.add(e);
            }
        }
        for (Entry e : traffic) {
            if (e.seq > afterSeq) {
                out.add(e);
            }
        }
        Collections.sort(out, BY_SEQ);
        return out;
    }

    private static final Comparator<Entry> BY_SEQ = new Comparator<Entry>() {
        @Override
        public int compare(Entry a, Entry b) {
            return a.seq < b.seq ? -1 : (a.seq == b.seq ? 0 : 1);
        }
    };

    /**
     * The {@code buffer} object of {@code hello}: {@code max_body_bytes} is the memory budget,
     * {@code body_bytes} the body bytes kept now (fewer than that: frames cost more than their bodies).
     */
    synchronized void writeStats(Json j) {
        j.obj().kv("max_txns", maxTxns).kv("max_body_bytes", maxBytes).kv("txns", txns.size())
                .kv("body_bytes", bodyBytes);
        if (firstSeq != 0) {
            j.kv("first_seq", firstSeq).kv("last_seq", lastSeq);
        }
        j.endObj();
    }
}
