package io.trafficpolice.sample

import io.grpc.CallOptions
import io.grpc.Channel
import io.grpc.KnownLength
import io.grpc.Metadata
import io.grpc.MethodDescriptor
import io.grpc.ServerServiceDefinition
import io.grpc.Status
import io.grpc.stub.AbstractBlockingStub
import io.grpc.stub.ClientCalls
import io.grpc.stub.ServerCalls
import java.io.ByteArrayInputStream
import java.io.ByteArrayOutputStream
import java.io.InputStream

/**
 * A small gRPC service, `sample.v1.Verification`, served inside the app: what an identity
 * verification SDK's backend could offer. Its messages are protobuf, encoded by hand (the sample
 * has no protoc step), and its stub looks like a generated blocking stub.
 */
object Rpc {
    /** Protobuf bytes as protobuf's own marshaller hands them out: a stream whose size is known. */
    private class Message(bytes: ByteArray) : ByteArrayInputStream(bytes), KnownLength

    private val bytes = object : MethodDescriptor.Marshaller<ByteArray> {
        override fun stream(value: ByteArray): InputStream = Message(value)
        override fun parse(stream: InputStream): ByteArray = stream.readBytes()
    }

    private fun method(type: MethodDescriptor.MethodType, name: String): MethodDescriptor<ByteArray, ByteArray> =
        MethodDescriptor.newBuilder<ByteArray, ByteArray>()
            .setType(type)
            .setFullMethodName(MethodDescriptor.generateFullMethodName("sample.v1.Verification", name))
            .setRequestMarshaller(bytes)
            .setResponseMarshaller(bytes)
            .build()

    val CHECK = method(MethodDescriptor.MethodType.UNARY, "Check")
    val WATCH = method(MethodDescriptor.MethodType.SERVER_STREAMING, "Watch")
    val LOOKUP = method(MethodDescriptor.MethodType.UNARY, "Lookup")

    private val REASON: Metadata.Key<String> = Metadata.Key.of("x-reason", Metadata.ASCII_STRING_MARSHALLER)

    private class Proto {
        val out = ByteArrayOutputStream()

        private fun varint(v: Long) {
            var x = v
            while (x >= 0x80) {
                out.write(((x and 0x7f) or 0x80).toInt())
                x = x ushr 7
            }
            out.write(x.toInt())
        }

        fun str(field: Int, s: String) {
            val b = s.toByteArray()
            varint((field.toLong() shl 3) or 2)
            varint(b.size.toLong())
            out.write(b)
        }

        fun num(field: Int, v: Long) {
            varint(field.toLong() shl 3)
            varint(v)
        }
    }

    private fun proto(build: Proto.() -> Unit): ByteArray = Proto().apply(build).out.toByteArray()

    /** `CheckRequest { session_id = 1, document = 2 }` */
    fun checkRequest(session: String) = proto { str(1, session); str(2, "passport") }

    /** `Verdict { session_id = 1, verdict = 2, score = 3 }` */
    fun verdict(session: String) = proto { str(1, session); str(2, "pass"); num(3, 97) }

    /** `WatchRequest { session_id = 1 }` */
    fun watchRequest(session: String) = proto { str(1, session) }

    /** `Progress { step = 1, percent = 2 }` */
    fun progress(step: String, percent: Long) = proto { str(1, step); num(2, percent) }

    /** `LookupRequest { document_id = 1 }` */
    fun lookupRequest(document: String) = proto { str(1, document) }

    /** The server side: a verdict, three progress updates, and a document it does not know. */
    fun service(): ServerServiceDefinition = ServerServiceDefinition.builder("sample.v1.Verification")
        .addMethod(CHECK, ServerCalls.asyncUnaryCall { _, out ->
            out.onNext(verdict("session_grpc"))
            out.onCompleted()
        })
        .addMethod(WATCH, ServerCalls.asyncServerStreamingCall { _, out ->
            for ((step, percent) in listOf("liveness" to 40L, "document" to 80L, "done" to 100L)) {
                out.onNext(progress(step, percent))
            }
            out.onCompleted()
        })
        .addMethod(LOOKUP, ServerCalls.asyncUnaryCall { _, out ->
            val trailers = Metadata()
            trailers.put(REASON, "unknown document")
            out.onError(Status.NOT_FOUND.withDescription("document doc_404 does not exist").asRuntimeException(trailers))
        })
        .build()

    /** What protoc's `VerificationGrpc.VerificationBlockingStub` would be. */
    class VerificationStub(channel: Channel, options: CallOptions = CallOptions.DEFAULT) :
        AbstractBlockingStub<VerificationStub>(channel, options) {
        override fun build(channel: Channel, callOptions: CallOptions) = VerificationStub(channel, callOptions)

        fun check(request: ByteArray): ByteArray = ClientCalls.blockingUnaryCall(channel, CHECK, callOptions, request)

        fun watch(request: ByteArray): Iterator<ByteArray> =
            ClientCalls.blockingServerStreamingCall(channel, WATCH, callOptions, request)

        fun lookup(request: ByteArray): ByteArray = ClientCalls.blockingUnaryCall(channel, LOOKUP, callOptions, request)
    }
}
