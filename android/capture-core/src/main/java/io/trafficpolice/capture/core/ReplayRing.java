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
 * kept whole and evicted oldest first when either limit is reached; diagnostics and traffic
 * samples are kept separately so start-up diagnostics are always replayed.
 */
final class ReplayRing {
    static final int MAX_DIAG = 200;
    static final int MAX_TRAFFIC = 240;

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
        long bodyBytes;
    }

    private final int maxTxns;
    private final long maxBodyBytes;
    private final LinkedHashMap<Long, TxnFrames> txns = new LinkedHashMap<>();
    private final ArrayDeque<Entry> diag = new ArrayDeque<>();
    private final ArrayDeque<Entry> traffic = new ArrayDeque<>();
    private long bodyBytes;
    private long firstSeq;
    private long lastSeq;

    ReplayRing(int maxTxns, long maxBodyBytes) {
        this.maxTxns = maxTxns;
        this.maxBodyBytes = maxBodyBytes;
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
                }
                t.frames.add(entry);
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
        while ((txns.size() > maxTxns || bodyBytes > maxBodyBytes) && it.hasNext()) {
            TxnFrames old = it.next().getValue();
            bodyBytes -= old.bodyBytes;
            it.remove();
        }
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

    /** The {@code buffer} object of {@code hello}. */
    synchronized void writeStats(Json j) {
        j.obj().kv("max_txns", maxTxns).kv("max_body_bytes", maxBodyBytes).kv("txns", txns.size())
                .kv("body_bytes", bodyBytes);
        if (firstSeq != 0) {
            j.kv("first_seq", firstSeq).kv("last_seq", lastSeq);
        }
        j.endObj();
    }
}
