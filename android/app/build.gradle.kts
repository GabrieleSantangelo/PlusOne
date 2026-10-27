plugins {
    id("com.android.application")
}

// The app has Plus One's version (plusone/__init__.py): 0.5.0 -> code 500.
val appVersion = Regex("__version__ = \"([0-9.]+)\"")
    .find(rootDir.resolve("../plusone/__init__.py").readText())!!.groupValues[1]
val (major, minor, patch) = appVersion.split(".").map { it.toInt() }

// Release builds are signed with the key CI gets from its secrets (or a
// local one): PLUSONE_KEYSTORE, PLUSONE_KEYSTORE_PASSWORD, PLUSONE_KEY_ALIAS.
val keystore = System.getenv("PLUSONE_KEYSTORE")

android {
    namespace = "io.github.gabrielesantangelo.plusone"
    compileSdk = 36

    defaultConfig {
        applicationId = "io.github.gabrielesantangelo.plusone"
        // Android 13: the S Pen tablets Samsung still updates (front-buffered
        // rendering itself needs 10).
        minSdk = 33
        targetSdk = 36
        versionCode = major * 10000 + minor * 100 + patch
        versionName = appVersion
        // WebRTC's native code: the tablets this is for are all 64-bit ARM
        // (the other ABIs would add some 30 MB).
        ndk { abiFilters += "arm64-v8a" }
    }

    signingConfigs {
        if (keystore != null) {
            create("release") {
                storeFile = file(keystore)
                storePassword = System.getenv("PLUSONE_KEYSTORE_PASSWORD")
                keyAlias = System.getenv("PLUSONE_KEY_ALIAS") ?: "plusone"
                keyPassword = System.getenv("PLUSONE_KEYSTORE_PASSWORD")
            }
        }
    }

    buildTypes {
        release {
            // R8: Kotlin, androidx and Material down to what the app uses.
            isMinifyEnabled = true
            isShrinkResources = true
            proguardFiles(getDefaultProguardFile("proguard-android-optimize.txt"), "proguard-rules.pro")
            if (keystore != null) signingConfig = signingConfigs.getByName("release")
        }
        // A release build signed with the debug key, to try R8's output on a
        // device that has the debug build installed.
        create("shrunk") {
            initWith(getByName("release"))
            signingConfig = signingConfigs.getByName("debug")
            matchingFallbacks += "release"
        }
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_21
        targetCompatibility = JavaVersion.VERSION_21
    }

    // The link's tests run on the JVM (src/test): Android's classes there
    // are stubs, which do nothing instead of failing (Log, say).
    testOptions.unitTests.isReturnDefaultValues = true
}

dependencies {
    // Ink drawn straight into the front buffer.
    implementation("androidx.graphics:graphics-core:1.0.4")
    // Material 3 Expressive: the look of Android 16, in the wallpaper's colours.
    implementation("com.google.android.material:material:1.14.0")
    // WebRTC (Google's libwebrtc, as LiveKit builds and publishes it): the
    // camera and the microphone for the PC, with its audio processing,
    // congestion control and retransmissions.
    implementation("io.github.webrtc-sdk:android:125.6422.07")

    testImplementation("junit:junit:4.13.2")
    // org.json as it is: Android's own is only stubs off the device.
    testImplementation("org.json:json:20240303")
}
