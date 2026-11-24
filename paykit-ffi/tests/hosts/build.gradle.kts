plugins {
    kotlin("jvm") version "2.3.21"
    application
}

repositories { mavenCentral() }

dependencies {
    implementation("net.java.dev.jna:jna:5.17.0")
    implementation("org.jetbrains.kotlinx:kotlinx-coroutines-core:1.10.2")
}

val checkoutTest = providers.gradleProperty("checkoutTest").map(String::toBoolean).orElse(false)

kotlin {
    jvmToolchain(17)
    compilerOptions.allWarningsAsErrors.set(true)
    sourceSets.main {
        kotlin.srcDirs(".", providers.gradleProperty("bindings").get())
        kotlin.include("WalletConsumer.kt", "com/synonym/paykit/paykit.kt")
        if (checkoutTest.get()) {
            kotlin.include("CheckoutConsumer.kt", "ConsumerWallet.kt")
            listOf("Inbox", "Payer", "Payee", "Lists", "Public", "Periods", "Allowance", "Executor")
                .forEach { kotlin.include("${it}Consumer.kt") }
        }
    }
}

application {
    mainClass.set(checkoutTest.map { checkout ->
        if (checkout) "com.synonym.paykit.tests.CheckoutConsumer"
        else "com.synonym.paykit.tests.WalletConsumerKt"
    })
    applicationDefaultJvmArgs = listOf("-Djna.library.path=" + providers.gradleProperty("nativeLibraryDir").get())
}
