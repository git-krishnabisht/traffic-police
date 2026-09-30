package io.trafficpolice.capture.core;

import java.util.List;
import java.util.Map;

/**
 * Attach mode (ARCHITECTURE.md §4.7): what the agent's hooks look like, read again for every
 * {@code hello}, because OkHttp may be loaded, and hooked, after the runtime started.
 */
public interface AttachState {
    /** Whether an OkHttp client class is hooked in this process. */
    boolean okhttp();

    /** Detected client versions, e.g. okhttp → 4.12.0 (null when absent). */
    void clients(Map<String, String> into);

    /** Every hook, for {@code hello.hooks} (PROTOCOL.md §7.2). */
    List<Hook> hooks();

    /** One hook's state. */
    final class Hook {
        public final String id;
        /** {@code class#method(descriptor)}. */
        public final String target;
        /** installed, pending, class_not_found, method_not_found, failed or other_loader. */
        public final String status;
        public final long hits;
        /** Why it failed (a JVMTI error name, say), or null. */
        public final String detail;

        public Hook(String id, String target, String status, long hits, String detail) {
            this.id = id;
            this.target = target;
            this.status = status;
            this.hits = hits;
            this.detail = detail;
        }
    }
}
