# Dirigent version server

The server stores immutable releases and channel pointers in S3. Artifacts remain private; download URLs redirect to short-lived presigned S3 URLs.

## Configuration

Standard AWS credential and region variables are supported, plus:

```text
DIRIGENT_S3_BUCKET             required
DIRIGENT_PUBLISH_TOKEN         required
DIRIGENT_PUBLIC_URL            default: https://dirigent.sebba.dev
DIRIGENT_BIND                  default: 127.0.0.1:8080
DIRIGENT_MAX_ARTIFACT_BYTES    default: 1073741824
```

Run it with:

```sh
cargo run -p dirigent_server --release
```

`GET /health` is available for deployment health checks. TLS is expected to be terminated by the reverse proxy in front of the service.

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

Stable versions are release SemVer values and cannot be republished. Nightly versions use `YYYYMMDDTHHMMSSZ`. A release must be newer than the channel's current release.
