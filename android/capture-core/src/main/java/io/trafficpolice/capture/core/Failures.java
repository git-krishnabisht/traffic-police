package io.trafficpolice.capture.core;

import java.io.IOException;
import java.net.ConnectException;
import java.net.ProtocolException;
import java.net.SocketTimeoutException;
import java.net.UnknownHostException;
import java.util.Arrays;
import java.util.Collections;
import java.util.List;

/** The exceptions a {@code fail} action throws (PROTOCOL.md §8.4). */
final class Failures {
    static final List<String> KINDS =
            Collections.unmodifiableList(Arrays.asList("timeout", "io", "protocol", "unknown_host", "connect"));

    private Failures() {}

    static IOException create(String kind, String message) {
        String m = message != null ? message : "simulated by traffic-police";
        switch (kind) {
            case "timeout":
                return new SocketTimeoutException(m);
            case "protocol":
                return new ProtocolException(m);
            case "unknown_host":
                return new UnknownHostException(m);
            case "connect":
                return new ConnectException(m);
            default:
                return new IOException(m);
        }
    }
}
