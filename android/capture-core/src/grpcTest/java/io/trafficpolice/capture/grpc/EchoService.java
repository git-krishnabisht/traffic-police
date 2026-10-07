package io.trafficpolice.capture.grpc;

import io.grpc.KnownLength;
import io.grpc.Metadata;
import io.grpc.MethodDescriptor;
import io.grpc.ServerServiceDefinition;
import io.grpc.Status;
import io.grpc.stub.ServerCalls;
import java.io.ByteArrayInputStream;
import java.io.ByteArrayOutputStream;
import java.io.IOException;
import java.io.InputStream;
import java.io.UncheckedIOException;
import java.nio.charset.StandardCharsets;

/** A small gRPC service for the tests, with text messages (no protobuf needed). */
final class EchoService {
    private EchoService() {}

    /** Text, read back like any stream. */
    static final MethodDescriptor.Marshaller<String> TEXT = new MethodDescriptor.Marshaller<String>() {
        @Override
        public InputStream stream(String value) {
            return new ByteArrayInputStream(value.getBytes(StandardCharsets.UTF_8));
        }

        @Override
        public String parse(InputStream stream) {
            return read(stream);
        }
    };

    /** Text whose size is known before reading it, as protobuf's streams are. */
    static final MethodDescriptor.Marshaller<String> KNOWN = new MethodDescriptor.Marshaller<String>() {
        @Override
        public InputStream stream(String value) {
            return new Known(value.getBytes(StandardCharsets.UTF_8));
        }

        @Override
        public String parse(InputStream stream) {
            return read(stream);
        }
    };

    static final class Known extends ByteArrayInputStream implements KnownLength {
        Known(byte[] b) {
            super(b);
        }
    }

    static String read(InputStream in) {
        try {
            ByteArrayOutputStream out = new ByteArrayOutputStream();
            byte[] b = new byte[4096];
            for (int n; (n = in.read(b)) >= 0; ) {
                out.write(b, 0, n);
            }
            return new String(out.toByteArray(), StandardCharsets.UTF_8);
        } catch (IOException e) {
            throw new UncheckedIOException(e);
        }
    }

    private static MethodDescriptor<String, String> method(MethodDescriptor.MethodType type, String name) {
        return MethodDescriptor.<String, String>newBuilder()
                .setType(type)
                .setFullMethodName(MethodDescriptor.generateFullMethodName("test.v1.Echo", name))
                .setRequestMarshaller(KNOWN)
                .setResponseMarshaller(TEXT)
                .build();
    }

    static final MethodDescriptor<String, String> SAY = method(MethodDescriptor.MethodType.UNARY, "Say");
    static final MethodDescriptor<String, String> FAIL = method(MethodDescriptor.MethodType.UNARY, "Fail");
    static final MethodDescriptor<String, String> COUNT = method(MethodDescriptor.MethodType.SERVER_STREAMING, "Count");
    static final MethodDescriptor<String, String> SLOW = method(MethodDescriptor.MethodType.UNARY, "Slow");

    static final Metadata.Key<String> DETAIL = Metadata.Key.of("x-detail", Metadata.ASCII_STRING_MARSHALLER);
    static final Metadata.Key<byte[]> TRACE = Metadata.Key.of("x-trace-bin", Metadata.BINARY_BYTE_MARSHALLER);

    static ServerServiceDefinition definition() {
        return ServerServiceDefinition.builder("test.v1.Echo")
                .addMethod(SAY, ServerCalls.asyncUnaryCall((String req, io.grpc.stub.StreamObserver<String> out) -> {
                    out.onNext("hello " + req);
                    out.onCompleted();
                }))
                .addMethod(FAIL, ServerCalls.asyncUnaryCall((String req, io.grpc.stub.StreamObserver<String> out) -> {
                    Metadata trailers = new Metadata();
                    trailers.put(DETAIL, "no " + req);
                    trailers.put(TRACE, new byte[] {1, 2, 3});
                    out.onError(Status.NOT_FOUND.withDescription("no such thing: " + req).asRuntimeException(trailers));
                }))
                .addMethod(COUNT, ServerCalls.asyncServerStreamingCall(
                        (String req, io.grpc.stub.StreamObserver<String> out) -> {
                            for (int i = 1; i <= 3; i++) {
                                out.onNext(req + " " + i);
                            }
                            out.onCompleted();
                        }))
                .addMethod(SLOW, ServerCalls.asyncUnaryCall((String req, io.grpc.stub.StreamObserver<String> out) -> {
                    // never answers: the client's deadline ends the call
                }))
                .build();
    }

    /** gRPC's framing of one message: flag 0, its length, its bytes. */
    static byte[] frame(String message) {
        byte[] m = message.getBytes(StandardCharsets.UTF_8);
        byte[] out = new byte[m.length + 5];
        out[1] = (byte) (m.length >>> 24);
        out[2] = (byte) (m.length >>> 16);
        out[3] = (byte) (m.length >>> 8);
        out[4] = (byte) m.length;
        System.arraycopy(m, 0, out, 5, m.length);
        return out;
    }
}
