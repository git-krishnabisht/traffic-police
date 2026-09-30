package io.trafficpolice.capture.core;

import static org.junit.Assert.assertEquals;
import static org.junit.Assert.assertFalse;
import static org.junit.Assert.assertNull;
import static org.junit.Assert.assertTrue;

import java.util.ArrayList;
import java.util.List;
import java.util.Map;
import org.junit.After;
import org.junit.Test;

/** Attach mode's hello (PROTOCOL.md §7.2): hooks and OkHttp as they are when each host connects. */
public final class AttachHelloTest {
    /** A fake agent whose OkHttp hook installs later. */
    static final class FakeAttach implements AttachState {
        volatile boolean okhttp;

        @Override
        public boolean okhttp() {
            return okhttp;
        }

        @Override
        public void clients(Map<String, String> into) {
            into.put("okhttp", okhttp ? "4.12.0" : null);
        }

        @Override
        public List<Hook> hooks() {
            List<Hook> out = new ArrayList<>();
            out.add(new Hook("url.openConnection", "java.net.URL#openConnection()Ljava/net/URLConnection;", "installed", 3, null));
            out.add(new Hook("okhttp.networkInterceptors", "okhttp3.OkHttpClient#networkInterceptors()Ljava/util/List;",
                    okhttp ? "installed" : "pending", 0, okhttp ? null : "okhttp3.OkHttpClient is not loaded yet"));
            return out;
        }
    }

    private CaptureRuntime rt;

    @After
    public void tearDown() {
        if (rt != null) {
            rt.stop();
        }
    }

    @SuppressWarnings("unchecked")
    @Test
    public void hello_reports_the_hooks_as_they_are_at_each_connection() throws Exception {
        FakeAttach attach = new FakeAttach();
        CaptureRuntime.Options o = new CaptureRuntime.Options();
        o.mode = "attach";
        o.attach = attach;
        o.instance = "0123456789abcdef0123456789abcdef";
        o.sampleTraffic = false;
        CaptureRuntime current = CaptureRuntime.current();
        if (current != null) {
            current.stop();
        }
        rt = CaptureRuntime.start(new TestPlatform(false), o);

        TestHost first = TestHost.connect(rt);
        TestHost.Msg hello = first.hello;
        // the start diag is replayed; okhttp_missing is not sent, since OkHttp can load later
        boolean started = false;
        for (TestHost.Msg m : first.replayed) {
            started |= "diag".equals(m.t()) && "started".equals(m.str("code"));
            assertFalse(m.json.toString(), "diag".equals(m.t()) && "okhttp_missing".equals(m.str("code")));
        }
        assertTrue("the start is replayed", started);
        first.close();
        assertEquals("attach", hello.obj("runtime").get("mode"));
        assertFalse(hello.list("capabilities").contains("okhttp"));
        assertNull(hello.obj("clients").get("okhttp"));
        List<Object> hooks = hello.list("hooks");
        assertEquals(2, hooks.size());
        Map<String, Object> url = (Map<String, Object>) hooks.get(0);
        assertEquals("url.openConnection", url.get("id"));
        assertEquals("installed", url.get("status"));
        assertEquals(3L, url.get("hits"));
        Map<String, Object> ok = (Map<String, Object>) hooks.get(1);
        assertEquals("pending", ok.get("status"));
        assertEquals("okhttp3.OkHttpClient is not loaded yet", ok.get("detail"));

        attach.okhttp = true;
        TestHost second = TestHost.connect(rt);
        hello = second.hello;
        second.close();
        assertTrue(hello.list("capabilities").contains("okhttp"));
        assertTrue(hello.list("capabilities").contains("okhttp_events"));
        assertEquals("4.12.0", hello.obj("clients").get("okhttp"));
        ok = (Map<String, Object>) hello.list("hooks").get(1);
        assertEquals("installed", ok.get("status"));
        assertFalse(ok.containsKey("detail"));
    }
}
