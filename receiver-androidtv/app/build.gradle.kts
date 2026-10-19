plugins {
    id("com.android.application")
    id("org.jetbrains.kotlin.android")
}

android {
    namespace = "com.redharegames.nanocast.receiver"
    compileSdk = 35

    defaultConfig {
        applicationId = "com.redharegames.nanocast.receiver"
        minSdk = 26
        targetSdk = 35
        versionCode = 1
        versionName = "0.1.0-spike"
    }

    buildTypes {
        release {
            isMinifyEnabled = false
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
