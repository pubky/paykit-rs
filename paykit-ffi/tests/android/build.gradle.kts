// Runs the generated Kotlin bindings and the test-wallet library on an Android device:
//   cargo ndk -t arm64-v8a --platform 28 -o "$OUT/jniLibs" build --locked --release -p paykit-ffi --features test-wallet --lib
//   gradle -p paykit-ffi/tests/android -Pbindings="$OUT/kotlin" -PjniLibs="$OUT/jniLibs" connectedAndroidTest
// with the bindings generated from a host library built with `test-wallet`.
plugins {
    id("com.android.library") version "9.3.0"
}

android {
    namespace = "com.synonym.paykit.tests"
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
