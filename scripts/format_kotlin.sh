#!/bin/bash
LDK_NODE_ANDROID_DIR="bindings/kotlin/ldk-node-android"

# Run ktlintFormat in ldk-node-android
(
  cd $LDK_NODE_ANDROID_DIR || exit 1
  ./gradlew ktlintFormat
)
