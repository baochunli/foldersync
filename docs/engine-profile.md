# Engine profile

The development pin is rclone 1.76.0, the latest stable release when the
manifest was recorded. The manifest records archive and executable SHA-256
values for macOS arm64, macOS amd64, and Linux amd64.
[Upstream release](https://github.com/rclone/rclone/releases/tag/v1.76.0) ·
[Official checksums](https://downloads.rclone.org/v1.76.0/SHA256SUMS)

Run `scripts/fetch-rclone.sh` to verify and install the pinned executable in
`.tools/rclone/1.76.0/`. The adapter resolves that file to an absolute path.
`FOLDERSYNC_RCLONE` can point to an explicitly selected pinned executable
for development.

**TODO** qualify the exact argument vectors, supported initialization
directions, access checks, deletion limits, and recovery profile before
enabling sync.
