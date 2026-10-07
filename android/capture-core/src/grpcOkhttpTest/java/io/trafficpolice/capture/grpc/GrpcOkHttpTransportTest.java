package io.trafficpolice.capture.grpc;

import static org.junit.Assert.assertEquals;
import static org.junit.Assert.assertNotNull;
import static org.junit.Assert.assertTrue;
import static org.junit.Assert.fail;

import io.grpc.CallOptions;
import io.grpc.InsecureServerCredentials;
import io.grpc.ManagedChannel;
import io.grpc.Server;
import io.grpc.ServerCredentials;
import io.grpc.Status;
import io.grpc.StatusRuntimeException;
import io.grpc.TlsServerCredentials;
import io.grpc.okhttp.OkHttpChannelBuilder;
import io.grpc.okhttp.OkHttpServerBuilder;
import io.grpc.stub.ClientCalls;
import io.trafficpolice.capture.core.CaptureRuntime;
import io.trafficpolice.capture.core.TestHost;
import io.trafficpolice.capture.core.TestPlatform;
import java.io.ByteArrayInputStream;
import java.nio.charset.StandardCharsets;
import java.util.List;
import java.util.Map;
import okhttp3.tls.HandshakeCertificates;
import okhttp3.tls.HeldCertificate;
import org.junit.After;
import org.junit.Before;
import org.junit.Test;

/**
 * gRPC over a real HTTP/2 transport (grpc-okhttp, as Android apps use it): the scheme, address
 * and TLS session from the transport, the headers as sent (with what gRPC adds below every
 * interceptor), and a trailers-only error.
 */
public final class GrpcOkHttpTransportTest {
    private final TestPlatform platform = new TestPlatform(false);
    private CaptureRuntime rt;
    private TestHost host;
    private Server server;
    private ManagedChannel channel;

    @Before
    public void setUp() throws Exception {
        rt = TestPlatform.runtime(platform, false);
        host = TestHost.connect(rt);
    }

    @After
    public void tearDown() throws Exception {
        if (channel != null) channel.shutdownNow();
        if (server != null) server.shutdownNow();
        host.close();
        rt.stop();
    }

    private void serve(ServerCredentials creds) throws Exception {
        server = OkHttpServerBuilder.forPort(0, creds).addService(EchoService.definition()).build().start();
    }

    @SuppressWarnings("unchecked")
    private static boolean hasHeader(TestHost.Msg m, String key, String name) {
        for (Object o : m.list(key)) {
            if (name.equals(((List<Object>) o).get(0))) return true;
        }
        return false;
    }

    @Test
    @SuppressWarnings("unchecked")
    public void plaintext() throws Exception {
        serve(InsecureServerCredentials.create());
        int port = server.getPort();
        channel = OkHttpChannelBuilder.forAddress("127.0.0.1", port).usePlaintext()
                .intercept(CaptureClientInterceptor.INSTANCE).build();
        assertEquals("hello ana", ClientCalls.blockingUnaryCall(channel, EchoService.SAY, CallOptions.DEFAULT, "ana"));
        TestHost.Msg req = host.await(m -> "req".equals(m.t()), 5_000);
        assertEquals("http://127.0.0.1:" + port + "/test.v1.Echo/Say", req.str("url"));
        Map<String, Object> remote = (Map<String, Object>) req.obj("conn").get("remote");
        assertEquals("127.0.0.1", remote.get("ip"));
        assertEquals((long) port, ((Number) remote.get("port")).longValue());
        // the stream's headers: gRPC adds grpc-accept-encoding below the interceptors
        assertTrue(req.toString(), hasHeader(req, "headers", "grpc-accept-encoding"));
        TestHost.Msg resp = host.await(m -> "resp".equals(m.t()) && m.txn == req.txn, 5_000);
        assertTrue(resp.toString(), hasHeader(resp, "headers", "content-type"));
        assertEquals("done", host.await(m -> "done".equals(m.t()) && m.txn == req.txn, 5_000).t());

        // a trailers-only error: its content-type with the response, its metadata as trailers
        try {
            ClientCalls.blockingUnaryCall(channel, EchoService.FAIL, CallOptions.DEFAULT, "x");
            fail();
        } catch (StatusRuntimeException e) {
            assertEquals(Status.Code.NOT_FOUND, e.getStatus().getCode());
        }
        TestHost.Msg req2 = host.await(m -> "req".equals(m.t()) && m.str("url").endsWith("/Fail"), 5_000);
        TestHost.Msg resp2 = host.await(m -> "resp".equals(m.t()) && m.txn == req2.txn, 5_000);
        assertTrue(resp2.toString(), hasHeader(resp2, "headers", "content-type"));
        TestHost.Msg done2 = host.await(m -> "done".equals(m.t()) && m.txn == req2.txn, 5_000);
        assertTrue(done2.toString(), hasHeader(done2, "trailers", "x-detail"));
        assertEquals("NOT_FOUND", done2.obj("grpc").get("status"));
        // the second call reused the connection
        assertEquals(Boolean.TRUE, req2.obj("conn").get("reused"));
    }

    @Test
    @SuppressWarnings("unchecked")
    public void tls() throws Exception {
        HeldCertificate cert = new HeldCertificate.Builder().addSubjectAlternativeName("localhost").build();
        serve(TlsServerCredentials.create(
                new ByteArrayInputStream(cert.certificatePem().getBytes(StandardCharsets.US_ASCII)),
                new ByteArrayInputStream(cert.privateKeyPkcs8Pem().getBytes(StandardCharsets.US_ASCII))));
        HandshakeCertificates trust = new HandshakeCertificates.Builder()
                .addTrustedCertificate(cert.certificate()).build();
        channel = OkHttpChannelBuilder.forAddress("localhost", server.getPort())
                .sslSocketFactory(trust.sslSocketFactory())
                .intercept(CaptureClientInterceptor.INSTANCE).build();
        assertEquals("hello tls", ClientCalls.blockingUnaryCall(channel, EchoService.SAY, CallOptions.DEFAULT, "tls"));
        TestHost.Msg req = host.await(m -> "req".equals(m.t()), 5_000);
        assertEquals("https://localhost:" + server.getPort() + "/test.v1.Echo/Say", req.str("url"));
        Map<String, Object> tls = (Map<String, Object>) req.obj("conn").get("tls");
        assertNotNull(req.toString(), tls);
        assertTrue(String.valueOf(tls.get("version")), String.valueOf(tls.get("version")).startsWith("TLSv1."));
        assertEquals(1, ((List<Object>) tls.get("peer")).size());
    }
}
