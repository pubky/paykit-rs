package com.synonym.paykit

import android.content.Context

/**
 * Initializes the Android TLS certificate verifier used by Paykit.
 *
 * Adapted from `paykit-ffi/bindings/android/lib/src/main/kotlin-manual/`.
 * The package, object and method names must stay as they are: they bind to
 * the JNI symbol `Java_com_synonym_paykit_PaykitAndroid_nativeInitialize`
 * exported by `libpaykit.so` (`paykit-ffi/src/lib.rs`). That function hands
 * the application context to `rustls-platform-verifier`, which checks server
 * certificates with the Android trust store. Until it runs, every HTTPS
 * request fails.
 *
 * `ReactNativePaykitModule` calls this once when React Native creates the
 * module, so apps do not call it themselves.
 */
public object PaykitAndroid {
    init {
        // ART only resolves `external fun` symbols in libraries loaded through
        // System.loadLibrary. libpaykit.so is also a DT_NEEDED dependency of
        // the TurboModule library, but that alone does not register it.
        System.loadLibrary("paykit")
    }

    @JvmStatic
    public fun initialize(context: Context): Boolean =
        nativeInitialize(context.applicationContext)

    @JvmStatic
    public fun initializeOrThrow(context: Context) {
        check(initialize(context)) {
            "failed to initialize Paykit Android platform verifier"
        }
    }

    @JvmStatic
    private external fun nativeInitialize(context: Context): Boolean
}
