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

// The version is [workspace.package] version in ../Cargo.toml, like the PC app and the release
// tag (v0.2.0 <-> 0.2.0): the APK on a release is always that version. versionCode = major*10000
// + minor*100 + patch, so every release is higher than the one before (needed to update over it).
// `-Pnanocast.version=0.0.9` builds an older one, to try updating (Updater.kt).
val appVersion: String = providers.gradleProperty("nanocast.version").orNull
    ?: rootProject.file("../Cargo.toml").readLines()
        .firstNotNullOfOrNull { Regex("""^\s*version\s*=\s*"([^"]+)"""").find(it)?.groupValues?.get(1) }
    ?: error("no version in ../Cargo.toml")
val appVersionCode: Int = appVersion.substringBefore('-').split('.').map { it.toInt() }
    .also { parts -> require(parts.size == 3 && parts.all { it in 0..99 }) { "version $appVersion is not x.y.z below 100" } }
    .let { (major, minor, patch) -> major * 10000 + minor * 100 + patch }

android {
    namespace = "com.redharegames.nanocast.receiver"
    compileSdk = 35

    defaultConfig {
        applicationId = "com.redharegames.nanocast.receiver"
        minSdk = 26
        targetSdk = 35
        versionCode = appVersionCode
        versionName = appVersion
        // Updater.kt: GitHub release APKs update themselves; debug builds and Play do not.
        buildConfigField("boolean", "SELF_UPDATE", "false")
    }

    buildFeatures {
        buildConfig = true
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
            buildConfigField("boolean", "SELF_UPDATE", "true")
        }
        // Google Play (`bundlePlay`): the store updates the app, so no self-update and no
        // REQUEST_INSTALL_PACKAGES (removed by src/play/AndroidManifest.xml).
        create("play") {
            initWith(getByName("release"))
            matchingFallbacks += "release"
            buildConfigField("boolean", "SELF_UPDATE", "false")
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
