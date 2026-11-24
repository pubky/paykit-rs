# JNA looks up native functions, callback methods and structure fields by name.
-keep class com.sun.jna.** { *; }
-keep class com.synonym.paykit.** extends com.sun.jna.Structure { *; }
-keep interface com.synonym.paykit.** extends com.sun.jna.Library { *; }
-keep interface com.synonym.paykit.** extends com.sun.jna.Callback { *; }
-keep class com.synonym.paykit.** implements com.sun.jna.Callback { *; }
-keepattributes RuntimeVisibleAnnotations

# JNA also ships bindings for desktop Java.
-dontwarn java.awt.Component
-dontwarn java.awt.GraphicsEnvironment
-dontwarn java.awt.HeadlessException
-dontwarn java.awt.Window
