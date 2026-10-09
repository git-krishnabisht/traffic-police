package io.trafficpolice.capture.core;

import static org.junit.Assert.assertEquals;

import org.junit.Test;

/**
 * The version the runtime reports in its hello (and doctor shows) is the one the library is
 * published as: Gradle passes the build's version in.
 */
public final class VersionTest {
    @Test
    public void theRuntimeReportsTheBuildsVersion() {
        assertEquals(System.getProperty("trafficpolice.version"), CaptureRuntime.VERSION);
    }
}
