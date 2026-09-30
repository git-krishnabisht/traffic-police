package io.trafficpolice.sample

import android.app.Activity
import android.os.Bundle
import android.view.ViewGroup
import android.widget.Button
import android.widget.LinearLayout
import android.widget.ScrollView
import android.widget.TextView
import io.trafficpolice.TrafficPolice
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.cancel
import kotlinx.coroutines.delay
import kotlinx.coroutines.isActive
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext

/**
 * Buttons for each scenario, "run all", and a polling loop for watching live traffic.
 * `adb shell am start -n io.trafficpolice.sample/.MainActivity --es run all` runs every scenario
 * (`--es run poll` starts polling).
 */
class MainActivity : Activity() {
    private val scope = CoroutineScope(SupervisorJob() + Dispatchers.Main)
    private lateinit var output: TextView
    private lateinit var scenarios: Scenarios
    private var polling: Job? = null

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        scenarios = Scenarios(applicationContext, ::log)
        val column = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            setPadding(32, 48, 32, 32)
        }
        val status = TextView(this).apply {
            text = if (TrafficPolice.isActive()) "capture: on (debug build)" else "capture: off"
            textSize = 16f
        }
        column.addView(status)
        column.addView(button("Run all scenarios") { runAll() })
        column.addView(button("Start polling") { startPolling() })
        column.addView(button("Stop polling") { stopPolling() })
        for ((name, run) in scenarios.all) {
            column.addView(button(name) { launchScenario(name, run) })
        }
        output = TextView(this).apply {
            textSize = 12f
            typeface = android.graphics.Typeface.MONOSPACE
        }
        column.addView(output)
        setContentView(ScrollView(this).apply { addView(column) })

        scope.launch {
            withContext(Dispatchers.IO) { Backend.start() }
            log("backend on 127.0.0.1:${Backend.http.port} (https ${Backend.https.port})")
            handle(intent)
        }
    }

    override fun onNewIntent(intent: android.content.Intent) {
        super.onNewIntent(intent)
        handle(intent)
    }

    /** `--es run all` runs every scenario; `poll` starts polling; `overhead` measures capture's cost. */
    private fun handle(intent: android.content.Intent?) {
        when (intent?.getStringExtra("run")) {
            "all" -> runAll()
            "poll" -> startPolling()
            "overhead" -> launchScenario("overhead measurement") { scenarios.overhead() }
        }
    }

    private fun button(label: String, onClick: () -> Unit) = Button(this).apply {
        text = label
        isAllCaps = false
        layoutParams = LinearLayout.LayoutParams(ViewGroup.LayoutParams.MATCH_PARENT, ViewGroup.LayoutParams.WRAP_CONTENT)
        setOnClickListener { onClick() }
    }

    private fun runAll() {
        scope.launch {
            withContext(Dispatchers.IO) { Backend.start() }
            scenarios.runAll()
        }
    }

    private fun launchScenario(name: String, run: suspend () -> Unit) {
        scope.launch {
            withContext(Dispatchers.IO) { Backend.start() }
            log("▶ $name")
            try {
                run()
            } catch (e: Exception) {
                log("  ${e.javaClass.simpleName}: ${e.message}")
            }
        }
    }

    /** A status poll every 1.5 s, like the SDK's verdict polling, until stopped. */
    private fun startPolling() {
        if (polling?.isActive == true) return
        log("polling every 1.5 s")
        polling = scope.launch(Dispatchers.IO) {
            Backend.start()
            val client = scenarios.client
            var n = 0
            while (isActive) {
                n++
                try {
                    client.newCall(
                        okhttp3.Request.Builder().url(Backend.http.url("/api/sdk/status?sessionId=poll_$n")).build()
                    ).execute().close()
                } catch (e: Exception) {
                    log("  poll failed: ${e.message}")
                }
                delay(1500)
            }
        }
    }

    private fun stopPolling() {
        polling?.cancel()
        polling = null
        log("polling stopped")
    }

    private fun log(line: String) {
        runOnUiThread { output.append(line + "\n") }
        android.util.Log.i("TrafficPoliceSample", line)
    }

    override fun onDestroy() {
        scope.cancel()
        super.onDestroy()
    }
}
