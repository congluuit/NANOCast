import java.util.Properties

plugins {
    id("com.android.application")
    id("org.jetbrains.kotlin.android")
}

// Release signing: keystore.properties (receiver-androidtv/keystore.properties, git-ignored)
// or env NANOCAST_KEYSTORE / NANOCAST_KEYSTORE_PASSWORD / NANOCAST_KEY_ALIAS / NANOCAST_KEY_PASSWORD.
// Neither present -> release is signed with the debug key (fine for local testing, NOT for Play Store).
val keystoreProps = Properties().apply {
    val f = rootProject.file("keystore.properties")
    if (f.exists()) f.inputStream().use { load(it) }
}
fun signingValue(prop: String, env: String): String? =
    keystoreProps.getProperty(prop) ?: System.getenv(env)

val releaseStoreFile = signingValue("storeFile", "NANOCAST_KEYSTORE")

android {
    namespace = "com.redharegames.nanocast.receiver"
    compileSdk = 35

    defaultConfig {
        applicationId = "com.redharegames.nanocast.receiver"
        minSdk = 26
        targetSdk = 35
        versionCode = 1
        versionName = "0.1.0"
    }

    signingConfigs {
        if (releaseStoreFile != null) {
            create("release") {
                storeFile = rootProject.file(releaseStoreFile)
                storePassword = signingValue("storePassword", "NANOCAST_KEYSTORE_PASSWORD")
                keyAlias = signingValue("keyAlias", "NANOCAST_KEY_ALIAS")
                keyPassword = signingValue("keyPassword", "NANOCAST_KEY_PASSWORD")
            }
        }
    }

    buildTypes {
        release {
            // Tiny app, no reflection into MediaCodec: R8 buys nothing and risks breaking codec classes.
            isMinifyEnabled = false
            isShrinkResources = false
            signingConfig = signingConfigs.findByName("release") ?: signingConfigs.getByName("debug")
        }
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }
}

kotlin {
    compilerOptions {
        jvmTarget.set(org.jetbrains.kotlin.gradle.dsl.JvmTarget.JVM_17)
    }
}
