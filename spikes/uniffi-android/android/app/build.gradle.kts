plugins {
    id("com.android.application")
    id("org.jetbrains.kotlin.plugin.compose")
}

android {
    namespace = "tf.went.beatseam.spike"
    compileSdk = 35

    defaultConfig {
        applicationId = "tf.went.beatseam.spike.uniffi"
        minSdk = 26
        targetSdk = 35
        versionCode = 1
        versionName = "0.0.0"
        // The Rust library is built separately by cargo-ndk into src/main/jniLibs (see ../../README.md).
        ndk { abiFilters += listOf("arm64-v8a") }
    }

    buildFeatures { compose = true }
    // Measure without the debuggable-app slowdown (still signed with the debug key).
    buildTypes { debug { isDebuggable = false } }
}

dependencies {
    // UniFFI's generated Kotlin uses JNA to call the Rust library.
    implementation("net.java.dev.jna:jna:5.17.0@aar")
    implementation("org.jetbrains.kotlinx:kotlinx-coroutines-android:1.10.2")

    implementation(platform("androidx.compose:compose-bom:2025.09.00"))
    implementation("androidx.compose.material3:material3")
    implementation("androidx.compose.ui:ui")
    implementation("androidx.activity:activity-compose:1.10.1")
}
