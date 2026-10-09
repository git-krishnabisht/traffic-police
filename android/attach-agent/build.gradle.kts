import java.util.Properties
import org.gradle.api.tasks.bundling.Jar
import org.gradle.api.tasks.compile.JavaCompile

plugins {
    alias(libs.plugins.android.library)
}

android {
    namespace = "io.trafficpolice.attach"
    compileSdk = 37
    ndkVersion = "28.2.13676358"

    defaultConfig {
        minSdk = 26
        ndk {
            abiFilters += listOf("arm64-v8a", "armeabi-v7a", "x86_64")
        }
        externalNativeBuild {
            cmake {
                cppFlags += listOf("-std=c++17", "-fexceptions", "-frtti")
                arguments += listOf("-DANDROID_STL=c++_static")
            }
        }
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_1_8
        targetCompatibility = JavaVersion.VERSION_1_8
    }

    externalNativeBuild {
        cmake {
            path = file("src/main/cpp/CMakeLists.txt")
            version = "3.22.1"
        }
    }
}

dependencies {
    implementation(project(":capture-core"))
}

tasks.withType<JavaCompile>().configureEach {
    options.compilerArgs.add("-Xlint:-options")
}

val localProperties = Properties().apply {
    val propertiesFile = rootProject.file("local.properties")
    if (propertiesFile.isFile) propertiesFile.inputStream().use(::load)
}
val sdkPath = providers.environmentVariable("ANDROID_HOME").orNull
    ?: providers.environmentVariable("ANDROID_SDK_ROOT").orNull
    ?: localProperties.getProperty("sdk.dir")
    ?: error("Set ANDROID_HOME or sdk.dir in android/local.properties")
val sdkDirectory = file(sdkPath)
val buildToolsDirectory = sdkDirectory.resolve("build-tools").listFiles()
    ?.filter { it.isDirectory }
    ?.maxByOrNull { it.name }
    ?: error("No Android build-tools are installed under $sdkDirectory")
val d8 = buildToolsDirectory.resolve("d8")
val androidJar = sdkDirectory.resolve("platforms/android-37.0/android.jar")

// the jar is named with the version (until 2026-10-09 the name was written here, as 0.1.0)
val captureCoreJar = project(":capture-core").layout.buildDirectory.file("libs/capture-core-${project(":capture-core").version}.jar")
val bootApiJar = tasks.register<Jar>("bootApiJar") {
    dependsOn(":capture-core:compileJava")
    archiveFileName.set("traffic-police-boot-api.jar")
    destinationDirectory.set(layout.buildDirectory.dir("generated/boot-api"))
    from(project(":capture-core").layout.buildDirectory.dir("classes/java/main")) {
        include("io/trafficpolice/capture/attach/ExitHandler.class")
    }
}
val captureClasses = project(":capture").layout.buildDirectory.dir(
    "intermediates/javac/release/compileReleaseJavaWithJavac/classes"
)
val bootClasses = layout.buildDirectory.dir(
    "intermediates/javac/release/compileReleaseJavaWithJavac/classes"
)
val bootClassesJar = tasks.register<Jar>("bootClassesJar") {
    dependsOn("compileReleaseJavaWithJavac")
    archiveFileName.set("traffic-police-boot-classes.jar")
    destinationDirectory.set(layout.buildDirectory.dir("generated/boot-api"))
    from(bootClasses)
}
val captureClassesJar = project(":capture").tasks.register<Jar>("attachRuntimeClassesJar") {
    dependsOn(":capture:compileReleaseJavaWithJavac")
    archiveFileName.set("traffic-police-runtime-classes.jar")
    destinationDirectory.set(project(":capture").layout.buildDirectory.dir("generated/attach-runtime"))
    from(captureClasses)
}
val bootDexOutput = layout.buildDirectory.dir("generated/boot-dex")
val runtimeDexOutput = layout.buildDirectory.dir("generated/runtime-dex")
val nativeReleaseLibraries = layout.buildDirectory.dir(
    "intermediates/stripped_native_libs/release/stripReleaseDebugSymbols/out/lib"
)

val generateBootDex = tasks.register<Exec>("generateBootDex") {
    dependsOn(bootClassesJar, bootApiJar)
    inputs.file(bootClassesJar.flatMap { it.archiveFile })
    inputs.file(bootApiJar.flatMap { it.archiveFile })
    inputs.file(androidJar)
    outputs.file(bootDexOutput.map { it.file("traffic-police-boot.dex") })
    doFirst {
        val out = bootDexOutput.get().asFile
        out.mkdirs()
        commandLine(
            d8.absolutePath,
            "--min-api", "26",
            "--lib", androidJar.absolutePath,
            "--output", out.absolutePath,
            bootClassesJar.get().archiveFile.get().asFile.absolutePath,
            bootApiJar.get().archiveFile.get().asFile.absolutePath,
        )
    }
    doLast {
        bootDexOutput.get().file("classes.dex").asFile.renameTo(
            bootDexOutput.get().file("traffic-police-boot.dex").asFile
        )
    }
}

// ExitHandler lives only in the boot dex: a second copy in the runtime dex would be a different
// type from the one Trampoline accepts, so the runtime must resolve it from the boot class loader
val runtimeCoreJar = tasks.register<Jar>("runtimeCoreJar") {
    dependsOn(":capture-core:jar")
    archiveFileName.set("traffic-police-runtime-core.jar")
    destinationDirectory.set(layout.buildDirectory.dir("generated/runtime-core"))
    from(zipTree(captureCoreJar)) {
        exclude("io/trafficpolice/capture/attach/**")
    }
}

val generateRuntimeDex = tasks.register<Exec>("generateRuntimeDex") {
    dependsOn(runtimeCoreJar, captureClassesJar, bootApiJar)
    inputs.file(runtimeCoreJar.flatMap { it.archiveFile })
    inputs.file(captureClassesJar.flatMap { it.archiveFile })
    inputs.file(bootApiJar.flatMap { it.archiveFile })
    inputs.file(androidJar)
    outputs.file(runtimeDexOutput.map { it.file("traffic-police-runtime.dex") })
    doFirst {
        val out = runtimeDexOutput.get().asFile
        out.mkdirs()
        commandLine(
            d8.absolutePath,
            "--min-api", "26",
            "--lib", androidJar.absolutePath,
            "--classpath", bootApiJar.get().archiveFile.get().asFile.absolutePath,
            "--output", out.absolutePath,
            runtimeCoreJar.get().archiveFile.get().asFile.absolutePath,
            captureClassesJar.get().archiveFile.get().asFile.absolutePath,
        )
    }
    doLast {
        runtimeDexOutput.get().file("classes.dex").asFile.renameTo(
            runtimeDexOutput.get().file("traffic-police-runtime.dex").asFile
        )
    }
}

tasks.register("agentArtifacts") {
    dependsOn("assembleRelease", generateBootDex, generateRuntimeDex)
    inputs.dir(nativeReleaseLibraries)
    inputs.file(bootDexOutput.map { it.file("traffic-police-boot.dex") })
    inputs.file(runtimeDexOutput.map { it.file("traffic-police-runtime.dex") })
    outputs.dir(layout.buildDirectory.dir("outputs/agent"))
    doLast {
        val out = layout.buildDirectory.dir("outputs/agent").get().asFile
        out.deleteRecursively()
        out.mkdirs()
        val nativeLibs = nativeReleaseLibraries.get().asFile
        for (abi in listOf("arm64-v8a", "armeabi-v7a", "x86_64")) {
            val abiOut = out.resolve(abi)
            abiOut.mkdirs()
            nativeLibs.resolve("$abi/libtrafficpolice_agent.so").copyTo(
                abiOut.resolve("libtrafficpolice_agent.so"), overwrite = true
            )
        }
        bootDexOutput.get().file("traffic-police-boot.dex").asFile.copyTo(
            out.resolve("traffic-police-boot.dex"), overwrite = true
        )
        runtimeDexOutput.get().file("traffic-police-runtime.dex").asFile.copyTo(
            out.resolve("traffic-police-runtime.dex"), overwrite = true
        )
    }
}
