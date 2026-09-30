package io.trafficpolice.sample

import android.content.Context
import android.content.Intent
import io.trafficpolice.TrafficPolice
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.delay
import kotlinx.coroutines.suspendCancellableCoroutine
import kotlinx.coroutines.withContext
import okhttp3.Call
import okhttp3.Callback
import okhttp3.MediaType.Companion.toMediaType
import okhttp3.MultipartBody
import okhttp3.OkHttpClient
import okhttp3.Request
import okhttp3.RequestBody
import okhttp3.RequestBody.Companion.toRequestBody
import okhttp3.Response
import retrofit2.Retrofit
import retrofit2.converter.scalars.ScalarsConverterFactory
import retrofit2.http.Body
import retrofit2.http.GET
import retrofit2.http.POST
import retrofit2.http.Query
import java.io.IOException
import java.net.HttpURLConnection
import java.net.URL
import java.util.concurrent.TimeUnit
import javax.net.ssl.HttpsURLConnection
import kotlin.coroutines.resume
import kotlin.coroutines.resumeWithException

/** The SDK-style API, called through Retrofit suspend functions. */
interface SdkApi {
    @POST("api/sdk/init")
    suspend fun init(@Body body: RequestBody): String

    @GET("api/sdk/challenge")
    suspend fun challenge(@Query("session") session: String): String

    @POST("api/sdk/attest")
    suspend fun attest(@Body body: RequestBody): String

    @POST("api/sdk/enroll")
    suspend fun enroll(@Body body: RequestBody): String

    @GET("api/sdk/status")
    suspend fun status(@Query("sessionId") sessionId: String): String
}

/** Every capture path, as named scenarios; [all] runs them in order. */
class Scenarios(private val context: Context, private val log: (String) -> Unit) {
    private val jsonType = "application/json; charset=utf-8".toMediaType()

    /** The app's client: our network interceptor and listener, as the README shows. */
    val client: OkHttpClient = OkHttpClient.Builder()
        .addNetworkInterceptor(TrafficPolice.networkInterceptor())
        .eventListenerFactory(TrafficPolice.eventListenerFactory())
        .readTimeout(10, TimeUnit.SECONDS)
        .build()

    private val base get() = Backend.http.url("/")

    private val api: SdkApi by lazy {
        Retrofit.Builder()
            .baseUrl(base)
            .client(client)
            .addConverterFactory(ScalarsConverterFactory.create())
            .build()
            .create(SdkApi::class.java)
    }

    val all: List<Pair<String, suspend () -> Unit>> = listOf(
        "SDK session (Retrofit suspend)" to ::sdkSession,
        "gzip JSON (OkHttp execute)" to ::gzipJson,
        "feed (OkHttp enqueue)" to ::enqueue,
        "HttpURLConnection GET and POST" to ::huc,
        "streaming response" to ::streaming,
        "5 MB download" to ::download,
        "multipart upload" to ::upload,
        "redirect" to ::redirect,
        "404 and 500" to ::errors,
        "timeout" to ::timeout,
        "cancelled call" to ::cancel,
        "image" to ::image,
        "protobuf" to ::protobuf,
        "HTTPS (OkHttp and HttpURLConnection)" to ::https,
        "unknown host" to ::unknownHost,
        "second process" to ::secondProcess,
    )

    suspend fun runAll() {
        for ((name, run) in all) {
            log("▶ $name")
            try {
                run()
            } catch (e: Exception) {
                log("  ${e.javaClass.simpleName}: ${e.message}")
            }
        }
        get("/done")
        log("✓ all scenarios finished")
    }

    private suspend fun get(path: String, c: OkHttpClient = client): String = withContext(Dispatchers.IO) {
        c.newCall(Request.Builder().url(base.resolve(path)!!).build()).execute().use { it.body.string() }
    }

    private suspend fun sdkSession() = withContext<Unit>(Dispatchers.IO) {
        val init = api.init("""{"sdkVersion":"2.4.1","platform":"android"}""".toRequestBody(jsonType))
        val session = Regex("\"sessionId\":\"([^\"]+)\"").find(init)!!.groupValues[1]
        api.challenge(session)
        api.attest("""{"session":"$session","integrity":"ok"}""".toRequestBody(jsonType))
        api.enroll("""{"session":"$session","document":"passport"}""".toRequestBody(jsonType))
        while (true) {
            val status = api.status(session)
            log("  status: ${Regex("\"verdict\":\"(\\w+)\"").find(status)?.groupValues?.get(1)}")
            if (!status.contains("\"pending\"")) break
            delay(1500)
        }
    }

    private suspend fun gzipJson() {
        val profile = get("/api/profile")
        log("  profile: ${profile.take(40)}…")
    }

    private suspend fun enqueue() = suspendCancellableCoroutine<Unit> { cont ->
        // enqueue() from this thread: the call runs on OkHttp's dispatcher, but the capture
        // records this thread and stack as the call site
        client.newCall(Request.Builder().url(base.resolve("/api/feed?page=2")!!).build()).enqueue(object : Callback {
            override fun onFailure(call: Call, e: IOException) = cont.resumeWithException(e)
            override fun onResponse(call: Call, response: Response) {
                val text = response.use { it.body.string() }
                log("  feed: ${text.take(40)}…")
                cont.resume(Unit)
            }
        })
    }

    private suspend fun huc() = withContext<Unit>(Dispatchers.IO) {
        val get = TrafficPolice.wrap(URL(base.resolve("/huc/config").toString()).openConnection() as HttpURLConnection)
        get.inputStream.use { it.readBytes() }
        val post = TrafficPolice.wrap(URL(base.resolve("/huc/submit").toString()).openConnection() as HttpURLConnection)
        post.requestMethod = "POST"
        post.doOutput = true
        post.setRequestProperty("Content-Type", "application/json")
        post.outputStream.use { it.write("""{"answer":42}""".toByteArray()) }
        log("  POST status ${post.responseCode}")
        post.inputStream.use { it.readBytes() }
        post.disconnect()
    }

    private suspend fun streaming() {
        val text = get("/stream")
        log("  streamed ${text.length} chars")
    }

    private suspend fun download() = withContext<Unit>(Dispatchers.IO) {
        client.newCall(Request.Builder().url(base.resolve("/download/model.bin")!!).build()).execute().use { r ->
            val source = r.body.source()
            var total = 0L
            val buf = ByteArray(16 * 1024)
            val stream = source.inputStream()
            while (true) {
                val n = stream.read(buf)
                if (n < 0) break
                total += n
            }
            log("  downloaded $total bytes")
        }
    }

    private suspend fun upload() = withContext<Unit>(Dispatchers.IO) {
        val photo = ByteArray(200 * 1024) { (it % 251).toByte() }
        val body = MultipartBody.Builder().setType(MultipartBody.FORM)
            .addFormDataPart("meta", null, """{"type":"selfie"}""".toRequestBody(jsonType))
            .addFormDataPart("image", "selfie.jpg", photo.toRequestBody("image/jpeg".toMediaType()))
            .build()
        client.newCall(Request.Builder().url(base.resolve("/upload")!!).post(body).build()).execute().use {
            log("  upload: ${it.body.string()}")
        }
    }

    private suspend fun redirect() {
        get("/old")
    }

    private suspend fun errors() {
        get("/missing")
        get("/error")
    }

    private suspend fun timeout() {
        val impatient = client.newBuilder().readTimeout(1, TimeUnit.SECONDS).retryOnConnectionFailure(false).build()
        get("/slow", impatient)
    }

    private suspend fun cancel() = withContext<Unit>(Dispatchers.IO) {
        val call = client.newCall(Request.Builder().url(base.resolve("/slow")!!).build())
        Thread { Thread.sleep(300); call.cancel() }.start()
        try {
            call.execute().close()
        } catch (e: IOException) {
            log("  cancelled: ${e.message}")
        }
    }

    private suspend fun image() {
        get("/avatar.png")
    }

    private suspend fun protobuf() = withContext<Unit>(Dispatchers.IO) {
        // field 1 = "dev-1", field 2 = { field 1 = "liveness.score", field 2 = 0.97 }
        val name = "liveness.score".toByteArray()
        val metric = byteArrayOf(0x0a, name.size.toByte()) + name + byteArrayOf(0x11) +
            java.nio.ByteBuffer.allocate(8).order(java.nio.ByteOrder.LITTLE_ENDIAN).putDouble(0.97).array()
        val payload = byteArrayOf(0x0a, 5) + "dev-1".toByteArray() + byteArrayOf(0x12, metric.size.toByte()) + metric
        client.newCall(
            Request.Builder().url(base.resolve("/metrics")!!)
                .post(payload.toRequestBody("application/x-protobuf".toMediaType())).build()
        ).execute().close()
    }

    private suspend fun https() = withContext<Unit>(Dispatchers.IO) {
        val trust = Backend.trust
        val tls = client.newBuilder().sslSocketFactory(trust.sslSocketFactory(), trust.trustManager).build()
        tls.newCall(Request.Builder().url(Backend.https.url("/secure")).build()).execute().use { it.body.string() }
        val conn = TrafficPolice.wrap(URL(Backend.https.url("/secure").toString()).openConnection() as HttpURLConnection)
        (conn as HttpsURLConnection).sslSocketFactory = trust.sslSocketFactory()
        conn.hostnameVerifier = javax.net.ssl.HostnameVerifier { host, _ -> host == "127.0.0.1" || host == "localhost" }
        conn.inputStream.use { it.readBytes() }
    }

    private suspend fun unknownHost() {
        withContext(Dispatchers.IO) {
            client.newCall(Request.Builder().url("https://api.nonexistent.invalid/v1/ping").build()).execute().close()
        }
    }

    /**
     * Capture's cost on this device: the same 2 KB GET through a plain client and through the
     * captured one, alternating, after a warm-up. Not part of [runAll].
     */
    suspend fun overhead() = withContext(Dispatchers.IO) {
        val plain = OkHttpClient.Builder().build()
        val url = Backend.bench.url("/bench")
        fun once(c: OkHttpClient): Long {
            val t0 = System.nanoTime()
            c.newCall(Request.Builder().url(url).build()).execute().use { it.body.bytes() }
            return System.nanoTime() - t0
        }
        repeat(300) {
            once(plain)
            once(client)
        }
        val n = 1000
        val without = LongArray(n)
        val with = LongArray(n)
        for (i in 0 until n) {
            without[i] = once(plain)
            with[i] = once(client)
        }
        without.sort()
        with.sort()
        fun us(v: Long) = "%.1f µs".format(v / 1000.0)
        log("overhead (n=$n, 2 KB GET): without capture median ${us(without[n / 2])}, p90 ${us(without[n * 9 / 10])}; " +
            "with capture median ${us(with[n / 2])}, p90 ${us(with[n * 9 / 10])}; " +
            "added ${us(with[n / 2] - without[n / 2])} at the median, ${us(with[n * 9 / 10] - without[n * 9 / 10])} at p90")
    }

    private suspend fun secondProcess() {
        context.startService(Intent(context, WorkerService::class.java).putExtra("port", Backend.http.port))
        delay(1500)
    }
}
