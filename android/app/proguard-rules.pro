# Default ProGuard rules for the release build. PocketVeto has no reflection
# beyond kotlinx.serialization's generated serializers, which ship their own
# consumer rules; nothing project-specific is required for v1.

-keepattributes *Annotation*
-keepattributes Signature
