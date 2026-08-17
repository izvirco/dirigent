# Dirigent version server

The server stores immutable releases and channel pointers under `dist/dirigent/` in S3. Artifacts are served directly from the public bucket through `cdn.sebba.dev`.

## Configuration

Standard AWS credential and region variables are supported, plus:

```text
DIRIGENT_S3_BUCKET             required
DIRIGENT_PUBLISH_TOKEN         required
DIRIGENT_CDN_URL               default: https://cdn.sebba.dev
DIRIGENT_BIND                  default: 127.0.0.1:8080
DIRIGENT_MAX_ARTIFACT_BYTES    default: 1073741824
```

Run it with:

```sh
cargo run -p dirigent_server --release
```

Pushes also publish `linux/amd64` containers to `ghcr.io/izvirco/dirigent-server`. Branch names, tags, and the commit SHA are emitted as image tags; the default branch additionally updates `latest`.

`GET /health` is available for deployment health checks. TLS is expected to be terminated by the reverse proxy in front of the service. The S3 bucket policy must allow public reads of `dist/dirigent/releases/*`; the server's credentials require read and write access.

## Website downloads

`GET /releases/{channel}/latest/{target}` redirects to the latest CDN artifact for that explicit target, so it can be used directly in links:

```html
<a href="https://dirigent.sebba.dev/releases/stable/latest/x86_64-pc-windows-msvc">Download for Windows</a>
<a href="https://dirigent.sebba.dev/releases/stable/latest/x86_64-arch-linux">Download for Arch Linux</a>
```

## Publishing

`POST /api/v0/version/{channel}` accepts a `manifest` JSON part and one file part per target. The target is both the manifest value and multipart field name.

```json
{
  "version": "0.0.0",
  "commit": "a1b2c3d4",
  "artifacts": [
    { "target": "x86_64-pc-windows-msvc", "file_name": "dirigent.exe" }
  ]
}
```

```sh
curl --fail-with-body \
  -H "Authorization: Bearer $DIRIGENT_PUBLISH_TOKEN" \
  -F 'manifest=@manifest.json;type=application/json' \
  -F 'x86_64-pc-windows-msvc=@dirigent.exe;filename=dirigent.exe' \
  https://dirigent.sebba.dev/api/v0/version/stable
```

Stable is a virtual channel advanced by SemVer tags, and a tagged version cannot be republished. Other channels use their branch name and `YYYYMMDDTHHMMSSZ` versions. A release must be newer than the channel's current release.
