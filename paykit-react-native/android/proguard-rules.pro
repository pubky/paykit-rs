# Consumer keep rules, applied when the app enables R8.

# The Rust TLS verifier (rustls-platform-verifier) calls these classes through
# JNI by name.
-keep, includedescriptorclasses class org.rustls.platformverifier.** { *; }

# libpaykit.so exports Java_com_synonym_paykit_PaykitAndroid_nativeInitialize.
-keepclasseswithmembernames, includedescriptorclasses class com.synonym.paykit.PaykitAndroid {
    native <methods>;
}
