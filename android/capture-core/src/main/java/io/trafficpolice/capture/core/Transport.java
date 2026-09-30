package io.trafficpolice.capture.core;

import java.io.Closeable;
import java.io.IOException;
import java.io.InputStream;
import java.io.OutputStream;

/** One accepted, authorized connection from a host (Android: a {@code LocalSocket}). */
public interface Transport extends Closeable {
    InputStream input() throws IOException;

    OutputStream output() throws IOException;

    /** Read timeout in milliseconds; 0 waits forever. */
    void setReadTimeout(int millis) throws IOException;
}
