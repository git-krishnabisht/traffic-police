// The library-mode artifact (io.trafficpolice:capture): the public TrafficPolice API, the
// auto-start provider and the Android side of the runtime (abstract socket server, clocks,
// traffic counters). Add it as debugImplementation, and capture-noop as releaseImplementation.

plugins {
    alias(libs.plugins.android.library)
    `maven-publish`
}

android {
    namespace = "io.trafficpolice"
    compileSdk = 37

    defaultConfig {
        // no newer than OkHttp's own floor, so any app that can use OkHttp can use this
        minSdk = 21
        consumerProguardFiles("consumer-rules.pro")
        // the device tests (src/androidTest): ./gradlew :capture:connectedDebugAndroidTest
        testInstrumentationRunner = "androidx.test.runner.AndroidJUnitRunner"
    }

    packaging {
        resources.excludes += listOf("META-INF/versions/9/module-info.class", "META-INF/*.kotlin_module")
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_1_8
        targetCompatibility = JavaVersion.VERSION_1_8
    }

    publishing {
        singleVariant("release") {
            withSourcesJar()
        }
    }
}

// Java 8 is the target on purpose (apps with old toolchains can consume it)
tasks.withType<JavaCompile>().configureEach {
    options.compilerArgs.add("-Xlint:-options")
}

dependencies {
    implementation(project(":capture-core"))
    compileOnly(libs.okhttp.baseline) { exclude(group = "com.squareup.okio") }
    compileOnly(libs.okio.baseline)
    compileOnly(libs.grpc.api.baseline)

    androidTestImplementation(testFixtures(project(":capture-core")))
    androidTestImplementation(libs.okhttp.latest)
    androidTestImplementation(libs.okhttp.tls)
    androidTestImplementation(libs.mockwebserver3)
    androidTestImplementation(libs.junit)
    androidTestImplementation(libs.androidx.test.runner)
    androidTestImplementation(libs.androidx.test.junit)
}

publishing {
    publications {
        create<MavenPublication>("release") {
            artifactId = "capture"
            afterEvaluate { from(components["release"]) }
        }
    }
}
