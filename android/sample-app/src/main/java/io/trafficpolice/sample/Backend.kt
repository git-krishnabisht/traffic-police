package io.trafficpolice.sample

import android.graphics.Bitmap
import android.graphics.Canvas
import android.graphics.Color
import android.graphics.Paint
import mockwebserver3.Dispatcher
import mockwebserver3.MockResponse
import mockwebserver3.MockWebServer
import mockwebserver3.RecordedRequest
import mockwebserver3.SocketEffect
import okhttp3.tls.HandshakeCertificates
import okhttp3.tls.HeldCertificate
import okhttp3.HttpUrl
import okhttp3.HttpUrl.Companion.toHttpUrl
import okio.Buffer
import java.io.ByteArrayOutputStream
import java.io.IOException
import java.net.InetAddress
import java.net.Socket
import java.util.concurrent.ConcurrentHashMap
import java.util.concurrent.TimeUnit
import java.util.concurrent.atomic.AtomicInteger
import java.util.zip.GZIPOutputStream
import kotlin.random.Random

/**
 * The sample's servers, inside the app: plain HTTP and HTTPS on 127.0.0.1. Every route answers
 * like a small identity-verification backend would, plus the odd cases the scenarios need.
 */
object Backend {
    lateinit var http: MockWebServer
        private set
    lateinit var https: TlsServer
        private set

    /** Certificates a client needs to trust the HTTPS server. */
    lateinit var trust: HandshakeCertificates
        private set

    private val statusPolls = ConcurrentHashMap<String, AtomicInteger>()
    private val sessionIds = AtomicInteger()

    @Synchronized
    fun start() {
        if (::http.isInitialized) return
        val loopback = InetAddress.getByName("127.0.0.1")
        http = MockWebServer().apply {
            dispatcher = Routes
            start(loopback, 0)
        }
        val cert = HeldCertificate.Builder()
            .commonName("localhost")
            .addSubjectAlternativeName("localhost")
            .addSubjectAlternativeName("127.0.0.1")
            .build()
        trust = HandshakeCertificates.Builder().addTrustedCertificate(cert.certificate).build()
        https = TlsServer(HandshakeCertificates.Builder().heldCertificate(cert).build(), loopback).apply { start() }
    }

    /**
     * A minimal HTTPS server that answers every request with `{"tls":true}`. MockWebServer's own
     * TLS path fails on Android 8: it reads the SNI names, which that Conscrypt reports as null
     * when the client sends none (it sends none for "localhost").
     */
    class TlsServer(certificates: HandshakeCertificates, address: InetAddress) {
        private val server = certificates.sslContext().serverSocketFactory.createServerSocket(0, 50, address)
        val port: Int get() = server.localPort

        fun url(path: String): HttpUrl = "https://localhost:$port$path".toHttpUrl()

        fun start() {
            Thread({
                while (true) {
                    val socket = try {
                        server.accept()
                    } catch (e: IOException) {
                        return@Thread
                    }
                    Thread({ serve(socket) }, "sample-https").apply { isDaemon = true }.start()
                }
            }, "sample-https-accept").apply { isDaemon = true }.start()
        }

        private fun serve(socket: Socket) {
            try {
                socket.use {
                    val input = it.getInputStream().bufferedReader(Charsets.ISO_8859_1)
                    input.readLine() ?: return
                    while (input.readLine()?.isNotEmpty() == true) {
                        // headers: the requests carry no body
                    }
                    val body = """{"tls":true}"""
                    it.getOutputStream().apply {
                        write(
                            ("HTTP/1.1 200 OK\r\nContent-Type: application/json; charset=utf-8\r\n" +
                                "Content-Length: ${body.length}\r\nServer: sample-backend\r\nConnection: close\r\n\r\n$body")
                                .toByteArray()
                        )
                        flush()
                    }
                }
            } catch (e: IOException) {
                // a client that went away
            }
        }
    }

    private fun json(code: Int, body: String) = MockResponse.Builder()
        .code(code)
        .addHeader("Content-Type", "application/json; charset=utf-8")
        .addHeader("X-Request-Id", java.util.UUID.randomUUID().toString())
        .body(body)

    private fun gzip(text: String): Buffer {
        val bytes = ByteArrayOutputStream()
        GZIPOutputStream(bytes).use { it.write(text.toByteArray()) }
        return Buffer().write(bytes.toByteArray())
    }

    private val avatar: ByteArray by lazy {
        val bitmap = Bitmap.createBitmap(96, 96, Bitmap.Config.ARGB_8888)
        val canvas = Canvas(bitmap)
        canvas.drawColor(Color.rgb(74, 158, 255))
        val paint = Paint(Paint.ANTI_ALIAS_FLAG).apply { color = Color.WHITE }
        canvas.drawCircle(48f, 36f, 18f, paint)
        canvas.drawCircle(48f, 96f, 34f, paint)
        ByteArrayOutputStream().also { bitmap.compress(Bitmap.CompressFormat.PNG, 100, it) }.toByteArray()
    }

    private val model: ByteArray by lazy { Random(7).nextBytes(5 * 1024 * 1024) }

    /** A tiny protobuf message: field 1 = 4 (varint), field 2 = "ok". */
    private val protobufAck = byteArrayOf(0x08, 0x04, 0x12, 0x02, 'o'.code.toByte(), 'k'.code.toByte())

    private object Routes : Dispatcher() {
        override fun dispatch(request: RecordedRequest): MockResponse {
            val url = request.url
            val response = when (url.encodedPath) {
                "/api/sdk/init" -> {
                    val id = "session_" + sessionIds.incrementAndGet()
                    json(200, """{"ok":true,"sessionId":"$id","config":{"pollIntervalMs":1500,"maxPolls":20}}""")
                }
                "/api/sdk/challenge" -> json(200, """{"nonce":"${java.util.UUID.randomUUID()}","expiresInMs":60000}""")
                "/api/sdk/attest" -> json(200, """{"ok":true,"attested":true,"riskScore":0}""")
                "/api/sdk/enroll" -> json(200, """{"ok":true,"enrolled":true}""")
                "/api/sdk/status" -> {
                    val session = url.queryParameter("sessionId") ?: "none"
                    val n = statusPolls.getOrPut(session) { AtomicInteger() }.incrementAndGet()
                    val verdict = if (n < 3) "pending" else "pass"
                    json(200, """{"ok":true,"sessionId":"$session","status":"${if (n < 3) "running" else "complete"}","verdict":"$verdict","poll":$n}""")
                }
                "/api/profile" -> MockResponse.Builder()
                    .addHeader("Content-Type", "application/json; charset=utf-8")
                    .addHeader("Content-Encoding", "gzip")
                    .addHeader("Vary", "Accept-Encoding")
                    .body(gzip("""{"id":4821,"name":"Asha Verma","plan":"pro","devices":[{"model":"Pixel 8"},{"model":"A015"}]}"""))
                "/api/feed" -> json(200, """{"page":${url.queryParameter("page") ?: 1},"items":[{"id":1},{"id":2},{"id":3}]}""")
                    .addHeader("Set-Cookie", "session=abc123; Path=/; HttpOnly")
                    .addHeader("Set-Cookie", "region=ap-south-1; Path=/")
                "/huc/config" -> json(200, """{"version":42,"flags":{"newModel":true}}""")
                "/huc/submit" -> json(201, """{"created":true,"bytes":${request.bodySize}}""")
                "/stream" -> MockResponse.Builder()
                    .addHeader("Content-Type", "text/plain; charset=utf-8")
                    .chunkedBody((1..400).joinToString("") { "event $it: status=running\n" }, 1024)
                    .throttleBody(2048, 100, TimeUnit.MILLISECONDS)
                "/download/model.bin" -> MockResponse.Builder()
                    .addHeader("Content-Type", "application/octet-stream")
                    .body(Buffer().write(model))
                    .throttleBody(256 * 1024, 250, TimeUnit.MILLISECONDS)
                "/upload" -> json(200, """{"received":${request.bodySize}}""")
                "/old" -> MockResponse.Builder().code(302).addHeader("Location", "/new")
                "/new" -> json(200, """{"moved":true}""")
                "/missing" -> json(404, """{"ok":false,"error":{"code":"NOT_FOUND"}}""")
                "/error" -> MockResponse.Builder().code(500)
                    .addHeader("Content-Type", "text/html; charset=utf-8")
                    .body("<!DOCTYPE html><html><body><h1>Internal Server Error</h1></body></html>")
                "/slow" -> MockResponse.Builder().onResponseStart(SocketEffect.Stall)
                "/avatar.png" -> MockResponse.Builder()
                    .addHeader("Content-Type", "image/png")
                    .addHeader("Cache-Control", "public, max-age=86400")
                    .body(Buffer().write(avatar))
                "/metrics" -> MockResponse.Builder()
                    .addHeader("Content-Type", "application/x-protobuf")
                    .body(Buffer().write(protobufAck))
                "/worker/ping" -> json(200, """{"from":"worker"}""")
                "/done" -> MockResponse.Builder().code(204)
                else -> json(404, """{"ok":false,"path":"${url.encodedPath}"}""")
            }
            return response.addHeader("Server", "sample-backend").build()
        }
    }
}
