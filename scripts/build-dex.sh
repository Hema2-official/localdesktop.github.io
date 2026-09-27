#!/bin/sh
# Rebuild src/android/dex/classes.dex from the Java sources in src/android/java, with a JDK and
# r8's d8 (https://dl.google.com/android/maven2/com/android/tools/r8/).
#
# Usage: scripts/build-dex.sh
# Environment: ANDROID_JAR (default: the android-33 jar under LOCALDESKTOP_DEV_DIR),
# R8_JAR (default: the newest r8-*.jar under LOCALDESKTOP_DEV_DIR/r8)
set -eu

cd "$(dirname "$0")/.."
dev_dir=${LOCALDESKTOP_DEV_DIR:-$HOME/.cache/localdesktop}
android_jar=${ANDROID_JAR:-$dev_dir/android-sdk/platforms/android-33/android.jar}
r8_jar=${R8_JAR:-$(ls "$dev_dir"/r8/r8-*.jar 2>/dev/null | sort -V | tail -n 1)}
if [ ! -f "$android_jar" ] || [ ! -f "$r8_jar" ]; then
    echo "Need android.jar ($android_jar) and an r8 jar (set R8_JAR)" >&2
    exit 1
fi

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
# Java 8 bytecode, which d8 turns into dex that runs on the app's minimum API level. No lambdas:
# android.jar lacks the LambdaMetafactory javac compiles them against.
javac -source 8 -target 8 -Xlint:-options -bootclasspath "$android_jar" -d "$work/classes" \
    $(find src/android/java -name '*.java')
java -cp "$r8_jar" com.android.tools.r8.D8 --release --min-api 21 --lib "$android_jar" \
    --output "$work" $(find "$work/classes" -name '*.class')
cp "$work/classes.dex" src/android/dex/classes.dex
echo "Wrote src/android/dex/classes.dex"
