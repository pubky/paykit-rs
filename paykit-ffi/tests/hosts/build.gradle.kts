plugins {
    kotlin("jvm") version "2.3.21"
    application
}

repositories { mavenCentral() }

dependencies {
    implementation("net.java.dev.jna:jna:5.17.0")
    implementation("org.jetbrains.kotlinx:kotlinx-coroutines-core:1.10.2")
}

kotlin {
    jvmToolchain(17)
    compilerOptions.allWarningsAsErrors.set(true)
    sourceSets.main {
        kotlin.srcDirs(".", providers.gradleProperty("bindings").get())
        kotlin.include("WalletConsumer.kt", "com/synonym/paykit/v2/paykit_v2.kt")
    }
}

application {
    mainClass.set("com.synonym.paykit.v2.tests.WalletConsumerKt")
    applicationDefaultJvmArgs = listOf("-Djna.library.path=" + providers.gradleProperty("nativeLibraryDir").get())
}
