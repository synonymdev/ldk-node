# Consumer keep rules for ldk-node Android bindings.
# Packaged into the AAR and applied automatically when a consuming app enables R8.

# JNA reads @Structure.FieldOrder and public fields by name, and constructs
# Structure, Structure$ByValue, and Structure$ByReference types reflectively.
-keepclassmembers class org.lightningdevkit.ldknode.** extends com.sun.jna.Structure {
    <fields>;
    <init>(...);
}

# JNA looks up Callback methods by name when building native function pointers.
-keepclassmembers class org.lightningdevkit.ldknode.** implements com.sun.jna.Callback {
    <methods>;
}

# UniFFI 0.28 loads UniffiLib via Native.load; JNA looks up interface methods by name.
-keep,includedescriptorclasses interface org.lightningdevkit.ldknode.UniffiLib {
    <methods>;
}

# @Structure.FieldOrder is read at runtime.
-keepattributes RuntimeVisibleAnnotations

# JNA's AAR references desktop AWT types that are absent on Android.
-dontwarn java.awt.Component
-dontwarn java.awt.GraphicsEnvironment
-dontwarn java.awt.HeadlessException
-dontwarn java.awt.Window
