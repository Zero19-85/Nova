plugins {
    id("com.android.application")
    id("org.jetbrains.kotlin.plugin.compose")
}

android {
    namespace = "com.nova.echo"
    compileSdk = 35

    defaultConfig {
        applicationId = "com.nova.echo"
        // API 26 floors us at Android 8.0. MediaCodec's low-latency path, HEVC
        // decode, and the NDK's own minimum all sit at or below this, and going
        // lower would buy devices that cannot decode 1080p60 anyway.
        minSdk = 26
        targetSdk = 35
        versionCode = 1
        versionName = "0.1"
        ndk { abiFilters += "arm64-v8a" }
    }

    buildTypes {
        release {
            isMinifyEnabled = false
            proguardFiles(getDefaultProguardFile("proguard-android-optimize.txt"), "proguard-rules.pro")
        }
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }
    buildFeatures { compose = true }

    // `src/main/jniLibs` is already the default location AGP looks in, and it is
    // where cargo-ndk writes libecho.so, so no sourceSets override is needed.

    packaging {
        // Keep the .so uncompressed and page-aligned. Required from Android 15
        // for 16 KB page devices, and it lets the loader mmap the library
        // instead of extracting it.
        jniLibs.useLegacyPackaging = false
    }
}

dependencies {
    implementation("androidx.core:core-ktx:1.13.1")
    implementation("androidx.activity:activity-compose:1.9.3")
    implementation(platform("androidx.compose:compose-bom:2024.10.01"))
    implementation("androidx.compose.ui:ui")
    implementation("androidx.compose.material3:material3")
    // Both arrive transitively through material3 today. Declared anyway because
    // this app uses them directly — `combinedClickable` for the host card's tap
    // and hold, and the gear glyph in the top bar — and a transitive dependency
    // that a future material3 drops takes the build with it.
    implementation("androidx.compose.foundation:foundation")
    implementation("androidx.compose.material:material-icons-core")
    implementation("androidx.lifecycle:lifecycle-runtime-ktx:2.8.7")
    debugImplementation("androidx.compose.ui:ui-tooling")
}

// ── Rust ────────────────────────────────────────────────────────────────────
// Building the native library from Gradle keeps one command ("build the app")
// truthful. Without this, a Kotlin change would rebuild and silently ship the
// previous libecho.so, which is the kind of staleness that costs an afternoon.
val cargoNdk = tasks.register<Exec>("cargoNdk") {
    workingDir = rootProject.projectDir.parentFile   // the cargo workspace root
    val out = "android/app/src/main/jniLibs"
    // `-P` (capital) is cargo-ndk's API level; lowercase `-p` is cargo's
    // --package and is passed through after `build`. cargo-ndk 4.x reuses the
    // lowercase form for packages too, so mixing them up reads as
    // "unknown package: 26".
    val args = listOf(
        "cargo", "ndk",
        "-t", "arm64-v8a",
        "-P", "26",
        "-o", out,
        "build", "--release",
        "-p", "echo-android",
    )
    commandLine(
        if (System.getProperty("os.name").startsWith("Windows")) listOf("cmd", "/c") + args
        else args
    )
}

tasks.named("preBuild") { dependsOn(cargoNdk) }
