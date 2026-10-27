# WebRTC calls into its Java classes from native code, by name.
-keep class org.webrtc.** { *; }
-dontwarn org.webrtc.**

# The link's core (libplusonelink, Rust) calls back into its signer by name.
-keep class io.github.gabrielesantangelo.plusone.link.LinkCore$Signer { *; }
