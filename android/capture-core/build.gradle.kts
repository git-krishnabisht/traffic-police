// The platform-independent capture runtime: protocol encoder, recorder, event queue, replay
// ring, connection handling, and the OkHttp and HttpURLConnection capture. Android specifics
// (LocalServerSocket, TrafficStats, the auto-start provider) live in :capture.
//
// Compiled as Java 8 against the oldest API it supports (OkHttp 3.14.9 with Okio 1.13.0; gRPC
// 1.21.0), so javac rejects anything newer; only ForwardingEventListener is compiled against
// OkHttp 5.5.0 (docs/research/05-okhttp-okio.md §10). No runtime dependencies.

plugins {
    `java-library`
    `java-test-fixtures`
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
    compileOnly(libs.grpc.api.baseline)
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
                    implementation(testFixtures(project()))
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

// The gRPC tests (src/grpcTest) on the oldest supported gRPC, the last before ClientStreamTracer's
// streamCreated (1.39), the first with it (1.40), and newer ones. gRPC's in-process transport is a
// separate artifact from 1.55; the newest suite also runs a real HTTP/2 transport (grpc-okhttp).
data class GrpcMatrix(val name: String, val grpc: String, val inprocess: Boolean, val okhttp: Boolean = false)

val grpcMatrix = listOf(
    GrpcMatrix("grpc1_21", "1.21.0", false),
    GrpcMatrix("grpc1_39", "1.39.0", false),
    GrpcMatrix("grpc1_40", "1.40.1", false),
    GrpcMatrix("grpc1_63", "1.63.0", true),
    GrpcMatrix("grpc1_84", "1.84.0", true, okhttp = true),
)

testing {
    suites {
        for (m in grpcMatrix) {
            register(m.name, JvmTestSuite::class) {
                useJUnit(libs.versions.junit.get())
                sources {
                    java.setSrcDirs(listOf("src/grpcTest/java") + if (m.okhttp) listOf("src/grpcOkhttpTest/java") else emptyList())
                }
                dependencies {
                    implementation(project())
                    implementation(testFixtures(project()))
                    implementation("io.grpc:grpc-api:${m.grpc}")
                    implementation("io.grpc:grpc-core:${m.grpc}")
                    implementation("io.grpc:grpc-stub:${m.grpc}")
                    if (m.inprocess) implementation("io.grpc:grpc-inprocess:${m.grpc}")
                    if (m.okhttp) {
                        implementation("io.grpc:grpc-okhttp:${m.grpc}")
                        implementation(libs.okhttp.tls)
                    }
                }
                targets.all {
                    testTask.configure {
                        systemProperty("trafficpolice.grpc", m.grpc)
                    }
                }
            }
        }
    }
}

tasks.named("check") {
    dependsOn(matrix.map { it.name })
    dependsOn(grpcMatrix.map { it.name })
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
    // CaptureRuntime.VERSION, which the runtime reports in its hello, must be the build's
    systemProperty("trafficpolice.version", project.version.toString())
    testLogging {
        events("failed")
        exceptionFormat = org.gradle.api.tasks.testing.logging.TestExceptionFormat.FULL
    }
}

// TestHost (the pretend host the JVM tests and the capture library's device tests connect with)
// is a test fixture, not part of the published library
val javaComponent = components["java"] as AdhocComponentWithVariants
javaComponent.withVariantsFromConfiguration(configurations["testFixturesApiElements"]) { skip() }
javaComponent.withVariantsFromConfiguration(configurations["testFixturesRuntimeElements"]) { skip() }

publishing {
    publications {
        create<MavenPublication>("release") {
            artifactId = "capture-core"
            from(components["java"])
        }
    }
}
