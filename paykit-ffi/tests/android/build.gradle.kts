// Runs the generated Kotlin bindings and the test-wallet library on an Android device.
plugins {
    id("com.android.library") version "9.3.0"
}

android {
    namespace = "com.synonym.paykit.v2.tests"
    compileSdk = 36
    defaultConfig {
        minSdk = 28
        testInstrumentationRunner = "androidx.test.runner.AndroidJUnitRunner"
    }
    sourceSets {
        getByName("main").jniLibs.srcDir(providers.gradleProperty("jniLibs").get())
        getByName("androidTest").kotlin.srcDir("../hosts")
        getByName("androidTest").kotlin.srcDir(providers.gradleProperty("bindings").get())
    }
}

dependencies {
    androidTestImplementation("net.java.dev.jna:jna:5.17.0@aar")
    androidTestImplementation("org.jetbrains.kotlinx:kotlinx-coroutines-android:1.10.2")
    androidTestImplementation("androidx.test:runner:1.7.0")
    androidTestImplementation("androidx.test.ext:junit:1.3.0")
}
