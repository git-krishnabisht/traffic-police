// The platform-independent capture runtime: protocol encoder, recorder, event queue, replay
// ring, connection handling, and the OkHttp and HttpURLConnection capture. Android specifics
// (LocalServerSocket, TrafficStats, the auto-start provider) live in :capture.
//
// Compiled as Java 8 against the oldest API it supports (OkHttp 3.14.9 with Okio 1.13.0), so
// javac rejects anything newer; only ForwardingEventListener is compiled against OkHttp 5.5.0
// (docs/research/05-okhttp-okio.md §10). No runtime dependencies.

plugins {
    `java-library`
    `maven-publish`
}

java {
    withSourcesJar()
}

tasks.withType<JavaCompile>().configureEach {
    options.release.set(8)
    options.encoding = "UTF-8"
    // "source value 8 is obsolete" is expected: Java 8 is the target on purpose
    options.compilerArgs.addAll(listOf("-Xlint:all", "-Xlint:-options", "-Xlint:-serial"))
    // production code is warning-free; tests call OkHttp APIs that newer versions deprecate
    if (name == "compileJava" || name == "compileOkhttp5Java") {
        options.compilerArgs.add("-Werror")
    } else {
        options.compilerArgs.add("-Xlint:-deprecation")
    }
}

// ForwardingEventListener overrides every EventListener callback up to OkHttp 5.5.0.
val okhttp5: SourceSet by sourceSets.creating

sourceSets.main {
    compileClasspath += okhttp5.output
    runtimeClasspath += okhttp5.output
    output.dir(mapOf("builtBy" to tasks.named(okhttp5.classesTaskName)), okhttp5.java.destinationDirectory)
}

dependencies {
    compileOnly(libs.okhttp.baseline) { exclude(group = "com.squareup.okio") }
    compileOnly(libs.okio.baseline)
    "okhttp5CompileOnly"(libs.okhttp.latest)
}

// The same tests run against every supported OkHttp major and minor that changed behaviour,
// each with the Okio it ships, plus newer Okio where our Okio 1.13 subset must still link.
data class Matrix(val name: String, val okhttp: String, val okio: String? = null)

val matrix = listOf(
    Matrix("okhttp3_9", "3.9.0"),
    Matrix("okhttp3_9_okio3", "3.9.0", "3.18.2"),
    Matrix("okhttp3_12", "3.12.13"),
    Matrix("okhttp3_14", "3.14.9"),
    Matrix("okhttp4_0", "4.0.0"),
    Matrix("okhttp4_12", "4.12.0"),
    Matrix("okhttp5_0", "5.0.0"),
    Matrix("okhttp5_5", "5.5.0"),
)

testing {
    suites {
        val test by getting(JvmTestSuite::class) {
            useJUnit(libs.versions.junit.get())
            dependencies {
                implementation("com.squareup.okhttp3:okhttp:4.12.0")
                implementation("com.squareup.okhttp3:mockwebserver:4.12.0")
            }
        }
        for (m in matrix) {
            register(m.name, JvmTestSuite::class) {
                useJUnit(libs.versions.junit.get())
                sources {
                    java.setSrcDirs(listOf("src/test/java"))
                    resources.setSrcDirs(listOf("src/test/resources"))
                }
                dependencies {
                    implementation(project())
                    implementation("com.squareup.okhttp3:okhttp:${m.okhttp}")
                    implementation("com.squareup.okhttp3:mockwebserver:${m.okhttp}")
                    if (m.okio != null) implementation("com.squareup.okio:okio:${m.okio}!!")
                }
                targets.all {
                    testTask.configure {
                        systemProperty("trafficpolice.okhttp", m.okhttp)
                        systemProperty("trafficpolice.okio", m.okio ?: "shipped")
                    }
                }
            }
        }
    }
}

tasks.named("check") {
    dependsOn(matrix.map { it.name })
}

// Protocol goldens are written only on request (PROTOCOL.md §11).
tasks.register<Test>("updateProtocolGoldens") {
    description = "Rewrites testdata/protocol/v1 from the Java encoder."
    val test = testing.suites.named<JvmTestSuite>("test").get()
    testClassesDirs = test.sources.output.classesDirs
    classpath = test.sources.runtimeClasspath
    systemProperty("trafficpolice.updateGoldens", "true")
    filter { includeTestsMatching("*ProtocolGoldenTest*") }
    outputs.upToDateWhen { false }
}

// What capture costs per request on the JVM (ARCHITECTURE.md §6); prints a table.
tasks.register<Test>("benchmarkOverhead") {
    description = "Measures the capture's cost per request on the JVM."
    val test = testing.suites.named<JvmTestSuite>("test").get()
    testClassesDirs = test.sources.output.classesDirs
    classpath = test.sources.runtimeClasspath
    systemProperty("trafficpolice.bench", "true")
    filter { includeTestsMatching("*OverheadBenchmark*") }
    testLogging { showStandardStreams = true }
    outputs.upToDateWhen { false }
}

tasks.withType<Test>().configureEach {
    systemProperty("trafficpolice.testdata", rootProject.projectDir.parentFile.resolve("testdata").absolutePath)
    testLogging {
        events("failed")
        exceptionFormat = org.gradle.api.tasks.testing.logging.TestExceptionFormat.FULL
    }
}

publishing {
    publications {
        create<MavenPublication>("release") {
            artifactId = "capture-core"
            from(components["java"])
        }
    }
}
