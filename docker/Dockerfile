# Build environment for the ntrack Android APK.
#
# Contains: JDK 17, Android SDK (platform 34 + build-tools), NDK r27,
# Gradle, stable Rust with Android targets, and cargo-ndk.
#
# Build the image:    docker build -t ntrack-builder docker/
# Then build the APK: see ../build.sh (mounts the repo at /work and runs
#                     scripts/build-apk.sh inside the container)

# Canonical's official Ubuntu images on ECR Public — same content as the
# Docker Hub `ubuntu` image but without anonymous pull rate limits.
# Override with: docker build --build-arg BASE_IMAGE=ubuntu:24.04 …
ARG BASE_IMAGE=public.ecr.aws/ubuntu/ubuntu:24.04
FROM ${BASE_IMAGE}

ENV DEBIAN_FRONTEND=noninteractive
RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates curl unzip zip git \
        openjdk-17-jdk-headless \
        build-essential pkg-config \
        libfontconfig1-dev \
    && rm -rf /var/lib/apt/lists/*

# Optional extra CA certificates (e.g. corporate TLS-inspecting proxies):
# drop PEM files named *.crt into docker/certs/ before building. No-op when
# the directory is empty. Installed into both the system store (curl, rust)
# and the JVM truststore (gradle, sdkmanager).
COPY certs/ /usr/local/share/ca-certificates/ntrack-extra/
RUN update-ca-certificates && \
    for c in /usr/local/share/ca-certificates/ntrack-extra/*.crt; do \
        if [ -f "$c" ]; then \
            keytool -importcert -cacerts -storepass changeit -noprompt \
                -alias "ntrack-$(basename "$c" .crt)" -file "$c" || exit 1; \
        fi; \
    done

# ---- Android SDK + NDK ------------------------------------------------------
ENV ANDROID_HOME=/opt/android-sdk
ENV ANDROID_SDK_ROOT=$ANDROID_HOME
ARG CMDLINE_TOOLS=11076708
ARG ANDROID_PLATFORM=34
ARG BUILD_TOOLS=34.0.0
ARG NDK_VERSION=27.2.12479018

RUN mkdir -p $ANDROID_HOME/cmdline-tools \
    && curl -fsSL -o /tmp/clt.zip \
        "https://dl.google.com/android/repository/commandlinetools-linux-${CMDLINE_TOOLS}_latest.zip" \
    && unzip -q /tmp/clt.zip -d $ANDROID_HOME/cmdline-tools \
    && mv $ANDROID_HOME/cmdline-tools/cmdline-tools $ANDROID_HOME/cmdline-tools/latest \
    && rm /tmp/clt.zip

ENV PATH=$ANDROID_HOME/cmdline-tools/latest/bin:$ANDROID_HOME/platform-tools:$PATH

RUN yes | sdkmanager --licenses > /dev/null \
    && sdkmanager \
        "platform-tools" \
        "platforms;android-${ANDROID_PLATFORM}" \
        "build-tools;${BUILD_TOOLS}" \
        "ndk;${NDK_VERSION}"

ENV ANDROID_NDK_HOME=$ANDROID_HOME/ndk/${NDK_VERSION}
ENV ANDROID_NDK_ROOT=$ANDROID_NDK_HOME

# ---- Gradle -----------------------------------------------------------------
ARG GRADLE_VERSION=8.11.1
RUN curl -fsSL -o /tmp/gradle.zip \
        "https://services.gradle.org/distributions/gradle-${GRADLE_VERSION}-bin.zip" \
    && unzip -q /tmp/gradle.zip -d /opt \
    && rm /tmp/gradle.zip \
    && ln -s /opt/gradle-${GRADLE_VERSION}/bin/gradle /usr/local/bin/gradle

# ---- Rust -------------------------------------------------------------------
ENV RUSTUP_HOME=/opt/rustup
ENV CARGO_HOME=/opt/cargo
ENV PATH=/opt/cargo/bin:$PATH

RUN curl -fsSL https://sh.rustup.rs | sh -s -- -y --profile minimal --default-toolchain stable \
    && rustup target add \
        aarch64-linux-android \
        armv7-linux-androideabi \
        x86_64-linux-android

ARG CARGO_NDK_VERSION=4.1.2
RUN cargo install cargo-ndk --version ${CARGO_NDK_VERSION} --locked

WORKDIR /work
CMD ["/bin/bash"]
