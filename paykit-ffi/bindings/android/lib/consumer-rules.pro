-keep,includedescriptorclasses class org.rustls.platformverifier.** { *; }

# Consumer keep rules for paykit Android bindings.
# Packaged into the AAR and applied automatically when a consuming app enables R8.

# JNA reads @Structure.FieldOrder and public fields by name, and constructs
# Structure, Structure$ByValue, and Structure$ByReference types reflectively.
-keepclassmembers class com.synonym.paykit.** extends com.sun.jna.Structure {
    <fields>;
    <init>(...);
}

# JNA looks up Callback methods by name when building native function pointers.
-keepclassmembers class com.synonym.paykit.** implements com.sun.jna.Callback {
    <methods>;
}

# Native.register maps remaining native methods by exact C symbol name.
-keepclasseswithmembers,allowshrinking,includedescriptorclasses class com.synonym.paykit.UniffiLib {
    native <methods>;
}
-keepclasseswithmembers,allowshrinking,includedescriptorclasses class com.synonym.paykit.IntegrityCheckingUniffiLib {
    native <methods>;
}

# JNA reads Structure.FieldOrder at runtime. R8 full mode strips that
# annotation unless the annotation type and annotated classes are kept.
-keepattributes RuntimeVisibleAnnotations
-keep,allowshrinking,allowoptimization class com.sun.jna.Structure$FieldOrder
-keep,allowshrinking,allowoptimization,allowobfuscation @com.sun.jna.Structure$FieldOrder class com.synonym.paykit.** {
    <fields>;
    <init>(...);
}

# libjnidispatch looks up Native/Structure/CallbackReference members by JNI name.
# Subclasses such as com.sun.jna.ptr.IntByReference are kept by the extends rules.
# See https://github.com/java-native-access/jna/blob/master/www/FrequentlyAskedQuestions.md#jna-on-android
-keep class com.sun.jna.* { *; }
-keep class * extends com.sun.jna.* { *; }
-keepclassmembers class * extends com.sun.jna.* { public *; }

# JNA's AAR references desktop AWT types that are absent on Android.
-dontwarn java.awt.Component
-dontwarn java.awt.GraphicsEnvironment
-dontwarn java.awt.HeadlessException
-dontwarn java.awt.Window
