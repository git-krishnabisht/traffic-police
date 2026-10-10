package io.trafficpolice.capture.core;

import static org.junit.Assert.assertEquals;
import static org.junit.Assert.assertFalse;

import org.junit.Test;

/** The thread a request started on, as PROTOCOL.md §4 writes it. */
public final class ThreadStackTest {
    private static String thread(ThreadStack s) {
        Json j = new Json();
        j.obj();
        s.writeThread(j);
        j.endObj();
        return j.toString();
    }

    @Test
    public void theKernelsThreadIdGoesWithTheThreadWhenThereIsOne() {
        StackTraceElement[] none = new StackTraceElement[0];
        assertEquals(
                "{\"thread\":{\"name\":\"OkHttp Dispatcher\",\"id\":57,\"tid\":4371,\"origin\":\"call\"}}",
                thread(new ThreadStack("OkHttp Dispatcher", 57, 4371, ThreadStack.ORIGIN_CALL, none, false)));
        // off Android (here) there is none to send
        assertEquals(-1, Tids.current());
        assertFalse(thread(ThreadStack.capture(ThreadStack.ORIGIN_CALL, 0)).contains("\"tid\""));
    }
}
