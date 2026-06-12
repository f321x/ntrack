# Extra CA certificates

If your network uses a TLS-inspecting proxy, place its CA certificate(s)
here as PEM files with a `.crt` extension before running `./build.sh`.
They are installed into the builder image's system and Java trust stores.

This directory is intentionally empty by default; certificates dropped in
here are git-ignored.
