#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
cd "$SCRIPT_DIR"

KEYSTORE="${1:-keystore.jks}"
KEYSTORE_PASSWORD="${KEYSTORE_PASSWORD:-noteoptimizer}"

echo "==> Building Docker image..."
docker build -t note-optimizer-builder "$SCRIPT_DIR"

# ── Generate keystore if it doesn't exist (inside Docker so keytool isn't needed locally) ──
if [ ! -f "$KEYSTORE" ]; then
    echo "==> Generating new signing keystore at $KEYSTORE"
    docker run --rm -v "$SCRIPT_DIR":/project note-optimizer-builder \
        keytool -genkeypair \
            -alias key0 \
            -keyalg RSA -keysize 2048 \
            -validity 10000 \
            -keystore "/project/$KEYSTORE" \
            -storepass "$KEYSTORE_PASSWORD" \
            -keypass "$KEYSTORE_PASSWORD" \
            -dname "CN=Note Optimizer, O=nrontsis"
fi

KEYSTORE_ABS="$(cd "$(dirname "$KEYSTORE")" && pwd)/$(basename "$KEYSTORE")"

# ── Build APK inside Docker ──
echo "==> Building Android APK..."

docker run --rm \
    -v "$SCRIPT_DIR":/project \
    -v "$KEYSTORE_ABS":/keystore.jks:ro \
    -e KEYSTORE_PATH=/keystore.jks \
    -e KEYSTORE_PASSWORD="$KEYSTORE_PASSWORD" \
    note-optimizer-builder \
    bash -c "
        chmod +x gradlew 2>/dev/null || true
        if [ ! -f gradlew ]; then
            gradle wrapper --gradle-version 8.11.1 --no-daemon
        fi
        ./gradlew assembleRelease --no-daemon
    "

APK_BUILD="app/build/outputs/apk/release/app-release.apk"
APK="NoteOptimizer.apk"
if [ -f "$APK_BUILD" ]; then
    mv "$APK_BUILD" "$APK"
    echo ""
    echo "==> APK built successfully: android/$APK"
    echo ""
    echo "==> SHA-256 signing fingerprint (for assetlinks.json):"
    docker run --rm -v "$KEYSTORE_ABS":/keystore.jks:ro note-optimizer-builder \
        keytool -list -v -keystore /keystore.jks -storepass "$KEYSTORE_PASSWORD" -alias key0 2>/dev/null \
        | grep "SHA256:" | sed 's/.*SHA256: //'
    echo ""
else
    echo "ERROR: APK not found at $APK" >&2
    exit 1
fi
